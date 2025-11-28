//! Test Helpers and Utilities
//!
//! This module provides common utilities, fixtures, and helper functions for integration tests.

use crate::{
    consensus::ConsensusMessage,
    consensus_manager::{config::ConsensusConfig, leader_manager::LeaderSelectionStrategy},
    crypto::aggregated::{BlsPublicKey, BlsSecretKey, PeerId},
    state::{peer::PeerSet, transaction::Transaction},
    storage::store::ConsensusStore,
};
use ark_serialize::CanonicalSerialize;
use rand::thread_rng;
use rtrb::{Consumer, Producer, RingBuffer};
use std::time::Duration;
use tempfile::TempDir;

/// Test configuration constants
pub const N: usize = 6;
pub const F: usize = 1;
pub const M_SIZE: usize = 3;
pub const BUFFER_SIZE: usize = 10_000;

/// Default view timeout for tests
pub const DEFAULT_VIEW_TIMEOUT: Duration = Duration::from_secs(5);

/// Default tick interval for consensus engines
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_millis(1);

/// Keypair for a replica
#[derive(Clone)]
pub struct KeyPair {
    pub secret_key: BlsSecretKey,
    pub public_key: BlsPublicKey,
}

impl KeyPair {
    pub fn generate() -> Self {
        let mut rng = thread_rng();
        let sk = BlsSecretKey::generate(&mut rng);
        let pk = sk.public_key();
        Self {
            secret_key: sk,
            public_key: pk,
        }
    }
}

/// Complete setup for a single replica including all communication channels and storage
pub struct ReplicaSetup<const N: usize, const F: usize, const M_SIZE: usize> {
    /// The replica's peer ID
    pub replica_id: PeerId,

    /// The replica's secret key
    pub secret_key: BlsSecretKey,

    /// Persistent storage for this replica
    pub storage: ConsensusStore,

    /// Consumer for incoming consensus messages (from network)
    pub message_consumer: Consumer<ConsensusMessage<N, F, M_SIZE>>,

    /// Producer for incoming consensus messages (network writes here)
    pub message_producer: Producer<ConsensusMessage<N, F, M_SIZE>>,

    /// Consumer for outgoing consensus messages (network reads from here)
    pub broadcast_consumer: Consumer<ConsensusMessage<N, F, M_SIZE>>,

    /// Producer for outgoing consensus messages (consensus engine writes here)
    pub broadcast_producer: Producer<ConsensusMessage<N, F, M_SIZE>>,

    /// Consumer for transactions (consensus engine reads from here)
    pub transaction_consumer: Consumer<Transaction>,

    /// Producer for transactions (clients write here)
    pub transaction_producer: Producer<Transaction>,

    /// Temporary directory for storage (must be kept alive)
    _temp_dir: TempDir,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> ReplicaSetup<N, F, M_SIZE> {
    /// Creates a new replica setup with all necessary components
    ///
    /// # Arguments
    /// * `replica_id` - The peer ID for this replica
    /// * `secret_key` - The BLS secret key for signing messages
    ///
    /// # Returns
    /// A complete replica setup with initialized storage and communication channels
    pub fn new(replica_id: PeerId, secret_key: BlsSecretKey) -> Self {
        // Create ring buffers for communication
        let (message_producer, message_consumer) = RingBuffer::new(BUFFER_SIZE);
        let (broadcast_producer, broadcast_consumer) = RingBuffer::new(BUFFER_SIZE);
        let (transaction_producer, transaction_consumer) = RingBuffer::new(BUFFER_SIZE);

        // Create temporary directory and storage
        let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");
        let db_path = temp_dir.path().join("consensus.redb");
        let storage = ConsensusStore::open(&db_path).expect("Failed to open storage");

        Self {
            replica_id,
            secret_key,
            storage,
            message_consumer,
            message_producer,
            broadcast_consumer,
            broadcast_producer,
            transaction_consumer,
            transaction_producer,
            _temp_dir: temp_dir,
        }
    }
}

/// Test fixture containing all components needed for an end-to-end test
pub struct TestFixture {
    /// Generated keypairs for all replicas
    pub keypairs: Vec<KeyPair>,

    /// The peer set containing all replica peer IDs
    pub peer_set: PeerSet,

    /// Base consensus configuration
    pub config: ConsensusConfig,
}

impl TestFixture {
    /// Creates a new test fixture with generated keypairs and configuration
    ///
    /// # Arguments
    /// * `n` - Number of replicas
    /// * `f` - Number of Byzantine faults tolerated
    /// * `view_timeout` - Timeout for view changes
    ///
    /// # Returns
    /// A complete test fixture ready for use
    pub fn new(n: usize, f: usize, view_timeout: Duration) -> Self {
        let mut keypairs = Vec::new();
        let mut public_keys = Vec::new();

        // Generate keypairs for all replicas
        for _ in 0..n {
            let keypair = KeyPair::generate();
            public_keys.push(keypair.public_key.clone());
            keypairs.push(keypair);
        }

        // Create peer set (automatically sorts peer IDs)
        let peer_set = PeerSet::new(public_keys);

        // Create config strings (hex-encoded public keys)
        let mut peer_strs = Vec::with_capacity(peer_set.sorted_peer_ids.len());
        for peer_id in &peer_set.sorted_peer_ids {
            let pk = peer_set.id_to_public_key.get(peer_id).unwrap();
            let mut buf = Vec::new();
            pk.0.serialize_compressed(&mut buf).unwrap();
            peer_strs.push(hex::encode(buf));
        }

        let config = ConsensusConfig {
            n,
            f,
            view_timeout,
            leader_manager: LeaderSelectionStrategy::RoundRobin,
            network: crate::consensus_manager::config::Network::Local,
            peers: peer_strs,
        };

        Self {
            keypairs,
            peer_set,
            config,
        }
    }

    /// Creates a default test fixture with standard parameters (N=6, F=1)
    pub fn default() -> Self {
        Self::new(N, F, DEFAULT_VIEW_TIMEOUT)
    }
}

/// Creates a test transaction with the given parameters and proper BLS signature
///
/// # Arguments
/// * `secret_key` - The secret key to sign the transaction with
/// * `sender` - The sender's public key
/// * `nonce` - The transaction nonce
/// * `payload` - The transaction payload (used to derive recipient and tx_hash)
pub fn create_test_transaction(
    secret_key: &crate::crypto::aggregated::BlsSecretKey,
    sender: &crate::crypto::aggregated::BlsPublicKey,
    nonce: u64,
    payload: Vec<u8>,
) -> Transaction {
    // Create a recipient address from the payload hash
    let mut recipient = [0u8; 32];
    let payload_hash = blake3::hash(&payload);
    recipient.copy_from_slice(&payload_hash.as_bytes()[..32]);

    // Compute transaction hash from the payload
    let tx_hash: [u8; blake3::OUT_LEN] = blake3::hash(&payload).into();

    // Get timestamp
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Create the transaction body to sign
    // We sign over all the transaction fields (excluding the signature itself)
    let mut message = Vec::new();
    message.extend_from_slice(&tx_hash);
    message.extend_from_slice(&recipient);
    message.extend_from_slice(&nonce.to_le_bytes());
    message.extend_from_slice(&timestamp.to_le_bytes());

    // Sign the transaction body
    let signature = secret_key.sign(&message);

    Transaction::new(
        sender.clone(),
        recipient,
        100, // amount
        nonce,
        timestamp,
        10, // fee
        tx_hash,
        signature,
    )
}

/// Creates multiple test transactions with proper signatures
///
/// # Arguments
/// * `keypairs` - The keypairs to use for signing (sender's secret key and public key)
/// * `count` - Number of transactions to create
pub fn create_test_transactions(keypairs: &[KeyPair], count: usize) -> Vec<Transaction> {
    let mut transactions = Vec::new();
    for i in 0..count {
        let keypair = &keypairs[i % keypairs.len()];
        let payload = format!("transaction-{}", i).into_bytes();
        transactions.push(create_test_transaction(
            &keypair.secret_key,
            &keypair.public_key,
            i as u64,
            payload,
        ));
    }
    transactions
}
