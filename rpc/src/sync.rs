//! Block synchronization for RPC nodes.
//!
//! RPC nodes sync finalized blocks from validators using the BLOCK_SYNC P2P channel.

use std::time::Duration;

use anyhow::Result;
use commonware_cryptography::ed25519;
use slog::Logger;

use consensus::state::block::Block;
use consensus::state::notarizations::LNotarization;
use consensus::state::peer::PeerSet;
use consensus::storage::store::ConsensusStore;
use p2p::config::{P2PConfig, ValidatorPeerInfo};
use p2p::message::{BlockRequest, BlockResponse};

use crate::RpcConfig;

/// Sync state for the RPC node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncState {
    /// Discovering the latest block height from validators.
    Discovering,
    /// Syncing blocks from `current` to `target` height.
    Syncing { current: u64, target: u64 },
    /// Fully synced and following new blocks in real-time.
    Following { height: u64 },
}

/// Block syncer configuration.
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// Interval between sync requests when catching up.
    pub sync_interval: Duration,
    /// Timeout for waiting for block responses.
    pub request_timeout: Duration,
    /// Number of blocks to request in parallel.
    pub batch_size: usize,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            sync_interval: Duration::from_millis(100),
            request_timeout: Duration::from_secs(5),
            batch_size: 10,
        }
    }
}

/// Block syncer that fetches finalized blocks from validators.
///
/// # Type Parameters
///
/// * `N` - Total number of validators (5f+1)
/// * `F` - Maximum faulty validators
pub struct BlockSyncer<const N: usize, const F: usize> {
    store: ConsensusStore,
    state: SyncState,
    config: SyncConfig,
    validators: Vec<ed25519::PublicKey>,
    peer_set: PeerSet,
    logger: Logger,
}

impl<const N: usize, const F: usize> BlockSyncer<N, F> {
    /// Create a new block syncer.
    pub fn new(
        store: ConsensusStore,
        validators: Vec<ed25519::PublicKey>,
        peer_set: PeerSet,
        config: SyncConfig,
        logger: Logger,
    ) -> Self {
        Self {
            store,
            state: SyncState::Discovering,
            config,
            validators,
            peer_set,
            logger,
        }
    }

    /// Get the current sync state.
    pub fn state(&self) -> &SyncState {
        &self.state
    }

    /// Get the sync interval for catching up.
    pub fn sync_interval(&self) -> Duration {
        self.config.sync_interval
    }

    /// Get the request timeout.
    pub fn request_timeout(&self) -> Duration {
        self.config.request_timeout
    }

    /// Get the batch size for parallel requests.
    pub fn batch_size(&self) -> usize {
        self.config.batch_size
    }

    /// Get a reference to the consensus store.
    pub fn store(&self) -> &ConsensusStore {
        &self.store
    }

    /// Get the local chain height (latest finalized block).
    pub fn local_height(&self) -> Result<u64> {
        match self.store.get_latest_finalized_block()? {
            Some(block) => Ok(block.height),
            None => Ok(0),
        }
    }

    /// Handle an incoming block response.
    pub fn handle_block_response(
        &mut self,
        response: BlockResponse,
        _from: ed25519::PublicKey,
    ) -> Result<Option<Block>> {
        match response {
            BlockResponse::Found {
                block_bytes,
                l_notarization_bytes,
            } => {
                // Deserialize the block
                let block: Block = rkyv::from_bytes::<Block, rkyv::rancor::Error>(&block_bytes)
                    .map_err(|e| anyhow::anyhow!("Failed to deserialize block: {:?}", e))?;

                let height = block.height;
                let hash = block.get_hash();

                slog::debug!(self.logger, "Received block";
                    "height" => height,
                    "hash" => hex::encode(&hash[..8]),
                );

                // Store the block
                self.store.put_finalized_block(&block)?;

                // Store L-notarization if provided
                if let Some(ref notarization_bytes) = l_notarization_bytes {
                    match rkyv::from_bytes::<LNotarization<N, F>, rkyv::rancor::Error>(
                        notarization_bytes,
                    ) {
                        Ok(l_notarization) => {
                            // Verify the L-notarization matches this block
                            if l_notarization.block_hash != hash {
                                slog::warn!(self.logger, "L-notarization block hash mismatch";
                                    "height" => height,
                                    "expected" => hex::encode(&hash[..8]),
                                    "got" => hex::encode(&l_notarization.block_hash[..8]),
                                );
                            } else if !l_notarization.verify(&self.peer_set) {
                                // Verify BLS aggregate signature before storing
                                slog::warn!(self.logger, "L-notarization signature verification failed";
                                    "height" => height,
                                    "view" => l_notarization.view,
                                    "signers" => l_notarization.peer_ids.len(),
                                );
                            } else if let Err(e) = self.store.put_l_notarization(&l_notarization) {
                                slog::warn!(self.logger, "Failed to store L-notarization";
                                    "height" => height,
                                    "error" => %e,
                                );
                            } else {
                                slog::debug!(self.logger, "Stored verified L-notarization";
                                    "height" => height,
                                    "view" => l_notarization.view,
                                    "signers" => l_notarization.peer_ids.len(),
                                );
                            }
                        }
                        Err(e) => {
                            slog::warn!(self.logger, "Failed to deserialize L-notarization";
                                "height" => height,
                                "error" => ?e,
                            );
                        }
                    }
                }

                // Update state
                self.update_state_after_block(height);

                Ok(Some(block))
            }
            BlockResponse::NotFound { view } => {
                slog::warn!(self.logger, "Block not found"; "view" => view);
                Ok(None)
            }
            BlockResponse::HashMismatch { view, actual_hash } => {
                slog::warn!(self.logger, "Block hash mismatch";
                    "view" => view,
                    "actual_hash" => ?actual_hash,
                );
                Ok(None)
            }
        }
    }

    /// Update sync state after receiving a block.
    fn update_state_after_block(&mut self, received_height: u64) {
        match &self.state {
            SyncState::Discovering => {
                // First block received, start syncing
                self.state = SyncState::Following {
                    height: received_height,
                };
            }
            SyncState::Syncing { current, target } => {
                let new_current = received_height.max(*current);
                if new_current >= *target {
                    slog::info!(self.logger, "Sync complete"; "height" => new_current);
                    self.state = SyncState::Following {
                        height: new_current,
                    };
                } else {
                    self.state = SyncState::Syncing {
                        current: new_current,
                        target: *target,
                    };
                }
            }
            SyncState::Following { height } => {
                if received_height > *height {
                    self.state = SyncState::Following {
                        height: received_height,
                    };
                }
            }
        }
    }

    /// Create a block request for the next needed height.
    pub fn next_block_request(&self) -> Option<BlockRequest> {
        let next_height = match &self.state {
            SyncState::Discovering => {
                // Request height 1 to start discovery
                1
            }
            SyncState::Syncing { current, target } => {
                if current < target {
                    current + 1
                } else {
                    return None;
                }
            }
            SyncState::Following { height } => {
                // Request next block
                height + 1
            }
        };

        Some(BlockRequest {
            view: next_height,
            block_hash: None,
        })
    }

    /// Set the target height when we learn it from a peer.
    pub fn set_target_height(&mut self, target: u64) {
        let current = self.local_height().unwrap_or(0);
        if target > current {
            slog::info!(self.logger, "Setting sync target";
                "current" => current,
                "target" => target,
            );
            self.state = SyncState::Syncing { current, target };
        } else {
            self.state = SyncState::Following { height: current };
        }
    }

    /// Check if we're fully synced.
    pub fn is_synced(&self) -> bool {
        matches!(self.state, SyncState::Following { .. })
    }

    /// Get a random validator to request from.
    pub fn pick_validator(&self) -> Option<ed25519::PublicKey> {
        if self.validators.is_empty() {
            return None;
        }
        // Simple round-robin would be better, but random works for now
        use rand::seq::SliceRandom;
        let mut rng = rand::thread_rng();
        self.validators.choose(&mut rng).cloned()
    }
}

/// Convert RpcConfig to P2PConfig for network initialization.
pub fn rpc_config_to_p2p(rpc_config: &RpcConfig) -> P2PConfig {
    P2PConfig {
        listen_addr: rpc_config.p2p_addr,
        external_addr: rpc_config.p2p_addr,
        validators: rpc_config.validators.clone(),
        total_number_peers: rpc_config.validators.len(),
        maximum_number_faulty_peers: 0, // RPC doesn't participate in consensus
        cluster_id: rpc_config.cluster_id.clone(),
        rpc_mode: true, // Mark as RPC node
        ..Default::default()
    }
}

/// Parse validator public keys from config.
pub fn parse_validator_keys(validators: &[ValidatorPeerInfo]) -> Vec<ed25519::PublicKey> {
    use commonware_codec::ReadExt;

    validators
        .iter()
        .filter_map(|v| {
            let bytes = v.parse_public_key_bytes()?;
            ed25519::PublicKey::read(&mut bytes.as_slice()).ok()
        })
        .collect()
}

/// Parse validator BLS public keys from config and construct a PeerSet.
///
/// Returns a PeerSet containing all validators whose BLS public keys could be parsed.
/// Validators without bls_public_key configured will be skipped.
pub fn parse_validator_peer_set(validators: &[ValidatorPeerInfo]) -> PeerSet {
    use ark_serialize::CanonicalDeserialize;
    use consensus::crypto::aggregated::BlsPublicKey;

    let bls_keys: Vec<BlsPublicKey> = validators
        .iter()
        .filter_map(|v| {
            let bytes = v.parse_bls_public_key_bytes()?;
            BlsPublicKey::deserialize_compressed(&*bytes).ok()
        })
        .collect();

    PeerSet::new(bls_keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use commonware_cryptography::Signer;
    use consensus::crypto::aggregated::{BlsSecretKey, BlsSignature};
    use consensus::state::block::Block;
    use tempfile::tempdir;

    fn test_logger() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    fn create_test_block(height: u64) -> Block {
        Block::new(
            height,                                      // view
            0,                                           // leader (PeerId)
            [0u8; 32],                                   // parent_block_hash
            vec![],                                      // transactions
            0,                                           // timestamp
            BlsSignature::aggregate(std::iter::empty()), // leader_signature
            true,                                        // is_finalized
            height,                                      // height
        )
    }

    fn create_test_syncer() -> (BlockSyncer<6, 1>, tempfile::TempDir) {
        let temp = tempdir().unwrap();
        let store = ConsensusStore::open(temp.path().join("test.redb")).unwrap();
        let logger = test_logger();
        let syncer = BlockSyncer::<6, 1>::new(
            store,
            vec![],
            PeerSet::new(vec![]),
            SyncConfig::default(),
            logger,
        );
        (syncer, temp)
    }

    #[test]
    fn test_sync_state_transitions() {
        let (mut syncer, _temp) = create_test_syncer();

        // Initial state
        assert_eq!(syncer.state(), &SyncState::Discovering);

        // Set target
        syncer.set_target_height(100);
        assert!(matches!(
            syncer.state(),
            SyncState::Syncing {
                current: 0,
                target: 100
            }
        ));

        // Simulate receiving blocks
        syncer.update_state_after_block(50);
        assert!(matches!(
            syncer.state(),
            SyncState::Syncing {
                current: 50,
                target: 100
            }
        ));

        syncer.update_state_after_block(100);
        assert!(matches!(
            syncer.state(),
            SyncState::Following { height: 100 }
        ));
    }

    #[test]
    fn test_set_target_height_already_synced() {
        let (mut syncer, _temp) = create_test_syncer();

        // If target is 0 (or less than current), should go to Following
        syncer.set_target_height(0);
        assert!(matches!(syncer.state(), SyncState::Following { height: 0 }));
    }

    #[test]
    fn test_update_state_from_discovering() {
        let (mut syncer, _temp) = create_test_syncer();
        assert!(matches!(syncer.state(), SyncState::Discovering));

        // First block should transition to Following
        syncer.update_state_after_block(1);
        assert!(matches!(syncer.state(), SyncState::Following { height: 1 }));
    }

    #[test]
    fn test_update_state_following_higher_block() {
        let (mut syncer, _temp) = create_test_syncer();
        syncer.update_state_after_block(10);
        assert!(matches!(
            syncer.state(),
            SyncState::Following { height: 10 }
        ));

        // Receiving higher block should update height
        syncer.update_state_after_block(15);
        assert!(matches!(
            syncer.state(),
            SyncState::Following { height: 15 }
        ));

        // Lower block should not change state
        syncer.update_state_after_block(5);
        assert!(matches!(
            syncer.state(),
            SyncState::Following { height: 15 }
        ));
    }

    #[test]
    fn test_next_block_request() {
        let (syncer, _temp) = create_test_syncer();

        // In discovering state, should request height 1
        let req = syncer.next_block_request().unwrap();
        assert_eq!(req.view, 1);
    }

    #[test]
    fn test_next_block_request_syncing() {
        let (mut syncer, _temp) = create_test_syncer();
        syncer.set_target_height(10);

        // Syncing from 0 to 10, should request 1
        let req = syncer.next_block_request().unwrap();
        assert_eq!(req.view, 1);

        // After receiving block 1
        syncer.update_state_after_block(1);
        let req = syncer.next_block_request().unwrap();
        assert_eq!(req.view, 2);
    }

    #[test]
    fn test_next_block_request_at_target() {
        let (mut syncer, _temp) = create_test_syncer();
        syncer.set_target_height(5);
        syncer.update_state_after_block(5);

        // Already at target, should be Following
        assert!(syncer.is_synced());
        let req = syncer.next_block_request().unwrap();
        assert_eq!(req.view, 6); // Request next block
    }

    #[test]
    fn test_next_block_request_following() {
        let (mut syncer, _temp) = create_test_syncer();
        syncer.update_state_after_block(100);

        // In following mode, should request height + 1
        let req = syncer.next_block_request().unwrap();
        assert_eq!(req.view, 101);
    }

    #[test]
    fn test_handle_block_response_found() {
        let (mut syncer, _temp) = create_test_syncer();
        let block = create_test_block(1);
        let block_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&block)
            .unwrap()
            .to_vec();

        let response = BlockResponse::Found {
            block_bytes,
            l_notarization_bytes: None,
        };
        let dummy_key = ed25519::PrivateKey::from_seed(42);
        let public_key = commonware_cryptography::Signer::public_key(&dummy_key);

        let result = syncer.handle_block_response(response, public_key);
        assert!(result.is_ok());
        let maybe_block = result.unwrap();
        assert!(maybe_block.is_some());
        assert_eq!(maybe_block.unwrap().height, 1);

        // State should have updated
        assert!(matches!(syncer.state(), SyncState::Following { height: 1 }));
    }

    #[test]
    fn test_handle_block_response_not_found() {
        let (mut syncer, _temp) = create_test_syncer();

        let response = BlockResponse::NotFound { view: 999 };
        let dummy_key = ed25519::PrivateKey::from_seed(42);
        let public_key = commonware_cryptography::Signer::public_key(&dummy_key);

        let result = syncer.handle_block_response(response, public_key);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());

        // State should remain Discovering
        assert!(matches!(syncer.state(), SyncState::Discovering));
    }

    #[test]
    fn test_handle_block_response_hash_mismatch() {
        let (mut syncer, _temp) = create_test_syncer();

        let response = BlockResponse::HashMismatch {
            view: 1,
            actual_hash: Some([0xAB; 32]),
        };
        let dummy_key = ed25519::PrivateKey::from_seed(42);
        let public_key = commonware_cryptography::Signer::public_key(&dummy_key);

        let result = syncer.handle_block_response(response, public_key);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_pick_validator_empty() {
        let (syncer, _temp) = create_test_syncer();
        assert!(syncer.pick_validator().is_none());
    }

    #[test]
    fn test_pick_validator_single() {
        let temp = tempdir().unwrap();
        let store = ConsensusStore::open(temp.path().join("test.redb")).unwrap();
        let logger = test_logger();

        let key = ed25519::PrivateKey::from_seed(100);
        let public_key = commonware_cryptography::Signer::public_key(&key);

        let syncer = BlockSyncer::<6, 1>::new(
            store,
            vec![public_key.clone()],
            PeerSet::new(vec![]),
            SyncConfig::default(),
            logger,
        );

        let picked = syncer.pick_validator();
        assert!(picked.is_some());
        assert_eq!(picked.unwrap(), public_key);
    }

    #[test]
    fn test_pick_validator_multiple() {
        let temp = tempdir().unwrap();
        let store = ConsensusStore::open(temp.path().join("test.redb")).unwrap();
        let logger = test_logger();

        let validators: Vec<_> = (0..5)
            .map(|i| {
                let key = ed25519::PrivateKey::from_seed(200 + i);
                commonware_cryptography::Signer::public_key(&key)
            })
            .collect();

        let syncer = BlockSyncer::<6, 1>::new(
            store,
            validators.clone(),
            PeerSet::new(vec![]),
            SyncConfig::default(),
            logger,
        );

        // Pick multiple times, should always be from the list
        for _ in 0..10 {
            let picked = syncer.pick_validator().unwrap();
            assert!(validators.contains(&picked));
        }
    }

    #[test]
    fn test_config_accessors() {
        let config = SyncConfig {
            sync_interval: Duration::from_millis(200),
            request_timeout: Duration::from_secs(10),
            batch_size: 20,
        };

        let temp = tempdir().unwrap();
        let store = ConsensusStore::open(temp.path().join("test.redb")).unwrap();
        let logger = test_logger();
        let syncer = BlockSyncer::<6, 1>::new(store, vec![], PeerSet::new(vec![]), config, logger);

        assert_eq!(syncer.sync_interval(), Duration::from_millis(200));
        assert_eq!(syncer.request_timeout(), Duration::from_secs(10));
        assert_eq!(syncer.batch_size(), 20);
    }

    #[test]
    fn test_is_synced() {
        let (mut syncer, _temp) = create_test_syncer();

        // Initially in Discovering, not synced
        assert!(!syncer.is_synced());

        // After setting target, in Syncing, not synced
        syncer.set_target_height(10);
        assert!(!syncer.is_synced());

        // After reaching target, synced
        syncer.update_state_after_block(10);
        assert!(syncer.is_synced());
    }

    #[test]
    fn test_rpc_config_to_p2p() {
        let rpc_config = RpcConfig {
            p2p_addr: "0.0.0.0:9001".parse().unwrap(),
            cluster_id: "test-cluster".to_string(),
            ..Default::default()
        };

        let p2p_config = rpc_config_to_p2p(&rpc_config);

        assert_eq!(p2p_config.listen_addr.port(), 9001);
        assert_eq!(p2p_config.cluster_id, "test-cluster");
        assert!(p2p_config.rpc_mode);
    }

    #[test]
    fn test_rpc_config_to_p2p_with_validators() {
        let rpc_config = RpcConfig {
            validators: vec![ValidatorPeerInfo {
                address: Some("10.0.0.1:9000".parse().unwrap()),
                ed25519_public_key:
                    "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
                bls_peer_id: 1,
                bls_public_key: None,
            }],
            ..Default::default()
        };

        let p2p_config = rpc_config_to_p2p(&rpc_config);

        assert_eq!(p2p_config.validators.len(), 1);
        assert_eq!(p2p_config.total_number_peers, 1);
    }

    #[test]
    fn test_parse_validator_keys_empty() {
        let keys = parse_validator_keys(&[]);
        assert!(keys.is_empty());
    }

    #[test]
    fn test_parse_validator_keys_valid() {
        // Generate a valid ed25519 key
        let key = ed25519::PrivateKey::from_seed(300);
        let public_key = commonware_cryptography::Signer::public_key(&key);
        let hex_key = hex::encode(public_key.as_ref());

        let validators = vec![ValidatorPeerInfo {
            address: Some("10.0.0.1:9000".parse().unwrap()),
            ed25519_public_key: hex_key,
            bls_peer_id: 1,
            bls_public_key: None,
        }];

        let keys = parse_validator_keys(&validators);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0], public_key);
    }

    #[test]
    fn test_parse_validator_keys_invalid_hex() {
        let validators = vec![ValidatorPeerInfo {
            address: Some("10.0.0.1:9000".parse().unwrap()),
            ed25519_public_key: "not_valid_hex".to_string(),
            bls_peer_id: 1,
            bls_public_key: None,
        }];

        // Invalid keys should be skipped
        let keys = parse_validator_keys(&validators);
        assert!(keys.is_empty());
    }

    #[test]
    fn test_parse_validator_keys_wrong_length() {
        let validators = vec![ValidatorPeerInfo {
            address: Some("10.0.0.1:9000".parse().unwrap()),
            ed25519_public_key: "0011223344".to_string(), // Too short
            bls_peer_id: 1,
            bls_public_key: None,
        }];

        let keys = parse_validator_keys(&validators);
        assert!(keys.is_empty());
    }

    #[test]
    fn test_handle_block_response_with_l_notarization() {
        // Generate BLS keys for N-F=5 validators (N=6, F=1)
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(12345);
        let mut bls_keys = Vec::new();
        let mut public_keys = Vec::new();
        for _ in 0..5 {
            let sk = BlsSecretKey::generate(&mut rng);
            public_keys.push(sk.public_key());
            bls_keys.push(sk);
        }

        // Create PeerSet with validator public keys
        let peer_set = PeerSet::new(public_keys.clone());
        let peer_ids: Vec<u64> = public_keys.iter().map(|pk| pk.to_peer_id()).collect();

        // Create syncer with proper PeerSet
        let temp = tempdir().unwrap();
        let store = ConsensusStore::open(temp.path().join("test.redb")).unwrap();
        let logger = test_logger();
        let mut syncer =
            BlockSyncer::<6, 1>::new(store, vec![], peer_set, SyncConfig::default(), logger);

        let block = create_test_block(1);
        let block_hash = block.get_hash();
        let block_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&block)
            .unwrap()
            .to_vec();

        // Create properly signed L-notarization
        // Sign the block hash with each validator's BLS key
        let signatures: Vec<_> = bls_keys.iter().map(|sk| sk.sign(&block_hash)).collect();
        let aggregated_signature =
            consensus::crypto::aggregated::BlsSignature::aggregate(signatures.iter());

        let l_notarization = LNotarization::<6, 1>::new(
            1,          // view (matches block)
            block_hash, // block_hash (must match)
            aggregated_signature,
            peer_ids.clone(), // peer_ids (5 signers = N-F)
            1,                // height (matches block)
        );
        let l_notarization_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&l_notarization)
            .unwrap()
            .to_vec();

        let response = BlockResponse::Found {
            block_bytes,
            l_notarization_bytes: Some(l_notarization_bytes),
        };
        let dummy_key = ed25519::PrivateKey::from_seed(42);
        let public_key = commonware_cryptography::Signer::public_key(&dummy_key);

        // Handle the response
        let result = syncer.handle_block_response(response, public_key);
        assert!(result.is_ok());
        let maybe_block = result.unwrap();
        assert!(maybe_block.is_some());
        assert_eq!(maybe_block.unwrap().height, 1);

        // Verify L-notarization was stored
        let stored: Option<LNotarization<6, 1>> = syncer
            .store()
            .get_l_notarization_by_height(1)
            .expect("Failed to get L-notarization");
        assert!(stored.is_some());

        let stored_notarization = stored.unwrap();
        assert_eq!(stored_notarization.height, 1);
        assert_eq!(stored_notarization.view, 1);
        assert_eq!(stored_notarization.block_hash, block_hash);
        assert_eq!(stored_notarization.peer_ids.len(), 5);
    }

    #[test]
    fn test_handle_block_response_mismatched_l_notarization() {
        let (mut syncer, _temp) = create_test_syncer();
        let block = create_test_block(1);
        let block_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&block)
            .unwrap()
            .to_vec();

        // Create L-notarization with WRONG block hash
        let wrong_hash = [0xAB; 32];
        let l_notarization = LNotarization::<6, 1>::new(
            1,
            wrong_hash, // Mismatched hash
            BlsSignature::aggregate(std::iter::empty()),
            vec![0, 1],
            1,
        );
        let l_notarization_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&l_notarization)
            .unwrap()
            .to_vec();

        let response = BlockResponse::Found {
            block_bytes,
            l_notarization_bytes: Some(l_notarization_bytes),
        };
        let dummy_key = ed25519::PrivateKey::from_seed(42);
        let public_key = commonware_cryptography::Signer::public_key(&dummy_key);

        // Handle should succeed (block stored) but L-notarization should not be stored
        let result = syncer.handle_block_response(response, public_key);
        assert!(result.is_ok());
        assert!(result.unwrap().is_some());

        // L-notarization should NOT be stored (hash mismatch)
        let stored: Option<LNotarization<6, 1>> = syncer
            .store()
            .get_l_notarization_by_height(1)
            .expect("Failed to query L-notarization");
        assert!(stored.is_none());
    }
}
