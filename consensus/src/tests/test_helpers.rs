//! Test Helpers and Utilities
//!
//! This module provides common utilities, fixtures, and helper functions for integration tests.

use crate::{
    consensus::ConsensusMessage,
    consensus_manager::{config::ConsensusConfig, leader_manager::LeaderSelectionStrategy},
    crypto::{
        aggregated::{BlsPublicKey, BlsSecretKey, PeerId},
        transaction_crypto::{TxPublicKey, TxSecretKey},
    },
    state::{address::Address, block::Block, peer::PeerSet, transaction::Transaction},
    storage::store::ConsensusStore,
    validation::{PendingStateWriter, ValidatedBlock, service::BlockValidationService},
};

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use ark_serialize::CanonicalSerialize;
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
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_millis(10);

/// Keypair for a replica
#[derive(Clone)]
pub struct KeyPair {
    pub secret_key: BlsSecretKey,
    pub public_key: BlsPublicKey,
}

impl KeyPair {
    pub fn generate() -> Self {
        let sk = BlsSecretKey::generate(&mut rand::thread_rng());
        let pk = sk.public_key();
        Self {
            secret_key: sk,
            public_key: pk,
        }
    }
}

fn gen_tx_keypair() -> (TxSecretKey, TxPublicKey) {
    let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
    let pk = sk.public_key();
    (sk, pk)
}

/// Complete setup for a single replica including BlockValidationService
pub struct ReplicaSetup<const N: usize, const F: usize, const M_SIZE: usize> {
    /// The replica's peer ID
    pub replica_id: PeerId,

    /// The replica's secret key
    pub secret_key: BlsSecretKey,

    /// Consumer for incoming consensus messages (from network)
    pub message_consumer: Consumer<ConsensusMessage<N, F, M_SIZE>>,

    /// Producer for incoming consensus messages (network writes here)
    pub message_producer: Producer<ConsensusMessage<N, F, M_SIZE>>,

    /// Consumer for outgoing consensus messages (network reads from here)
    pub broadcast_consumer: Consumer<ConsensusMessage<N, F, M_SIZE>>,

    /// Producer for outgoing consensus messages (consensus engine writes here)
    pub broadcast_producer: Producer<ConsensusMessage<N, F, M_SIZE>>,

    /// Producer for blocks to validate (P2P/leader sends blocks here)
    pub block_producer: Producer<Block>,

    /// Consumer for validated blocks (consensus engine reads from here)
    pub validated_block_consumer: Consumer<ValidatedBlock>,

    /// Persistence writer for consensus state
    pub persistence_writer: PendingStateWriter,

    /// Persistent storage for this replica
    pub storage: Arc<ConsensusStore>,

    /// Block validation service handle
    pub validation_service: BlockValidationService,

    /// Shutdown flag (shared with validation service)
    pub _shutdown: Arc<AtomicBool>,

    /// Producer for submitting transactions (clients write here)
    pub transaction_producer: Producer<Transaction>,

    /// Consumer for reading transactions (block builder reads here)
    pub _transaction_consumer: Consumer<Transaction>,

    /// Temporary directory for storage (must be kept alive)
    _temp_dir: TempDir,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> ReplicaSetup<N, F, M_SIZE> {
    /// Creates a new replica setup with BlockValidationService
    pub fn new(replica_id: PeerId, secret_key: BlsSecretKey, logger: slog::Logger) -> Self {
        // Create ring buffers for consensus messages
        let (message_producer, message_consumer) = RingBuffer::new(BUFFER_SIZE);
        let (broadcast_producer, broadcast_consumer) = RingBuffer::new(BUFFER_SIZE);

        // Create ring buffers for transactions
        let (transaction_producer, transaction_consumer) = RingBuffer::new(BUFFER_SIZE);

        // Create temporary directory and storage
        let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");
        let db_path = temp_dir.path().join("consensus.redb");
        let storage = Arc::new(ConsensusStore::open(&db_path).expect("Failed to open storage"));

        // Create shutdown flag
        let shutdown = Arc::new(AtomicBool::new(false));

        // Spawn BlockValidationService - creates block channels and pending state
        let (validation_service, block_producer, validated_block_consumer, persistence_writer) =
            BlockValidationService::spawn(
                Arc::clone(&storage),
                0, // last_finalized_view
                Arc::clone(&shutdown),
                logger,
            );

        Self {
            replica_id,
            secret_key,
            storage,
            message_consumer,
            message_producer,
            broadcast_consumer,
            broadcast_producer,
            block_producer,
            validated_block_consumer,
            persistence_writer,
            validation_service,
            _shutdown: shutdown,
            transaction_producer,
            _transaction_consumer: transaction_consumer,
            _temp_dir: temp_dir,
        }
    }

    /// Submit a transaction to this replica's mempool
    pub fn _submit_transaction(&mut self, tx: Transaction) -> Result<(), anyhow::Error> {
        self.transaction_producer
            .push(tx)
            .map_err(|e| anyhow::anyhow!("Failed to submit transaction: {:?}", e))
    }

    /// Submit a block for validation (called when receiving block from P2P or when proposing)
    pub fn _submit_block_for_validation(&mut self, block: Block) -> Result<(), anyhow::Error> {
        self.block_producer
            .push(block)
            .map_err(|e| anyhow::anyhow!("Failed to submit block: {:?}", e))
    }

    /// Build a block proposal from pending transactions (when this replica is leader)
    pub fn _build_block_proposal(
        &mut self,
        view: u64,
        parent_hash: [u8; 32],
        height: u64,
    ) -> Block {
        // Take transactions from pool
        let mut transactions = Vec::new();
        while let Ok(tx) = self._transaction_consumer.pop() {
            transactions.push(tx);
        }

        // Create unsigned block first to get hash
        let temp_block = Block::new(
            view,
            self.replica_id,
            parent_hash,
            transactions.clone(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            self.secret_key.sign(b"temp"),
            false,
            height,
        );

        let block_hash = temp_block.get_hash();

        // Sign with actual block hash
        Block::new(
            view,
            self.replica_id,
            parent_hash,
            transactions,
            temp_block.header.timestamp,
            self.secret_key.sign(&block_hash),
            false,
            height,
        )
    }

    /// Shutdown this replica's validation service
    pub fn _shutdown_validation(&mut self) {
        self._shutdown.store(true, Ordering::Release);
        self.validation_service.shutdown();
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
    secret_key: &TxSecretKey,
    public_key: &TxPublicKey,
    nonce: u64,
    payload: Vec<u8>,
) -> Transaction {
    // Create a recipient address from the payload hash
    let mut recipient = [0u8; 32];
    let payload_hash = blake3::hash(&payload);
    recipient.copy_from_slice(&payload_hash.as_bytes()[..32]);

    Transaction::new_transfer(
        Address::from_public_key(public_key),
        Address::from_bytes([2u8; 32]),
        100,
        nonce,
        10,
        secret_key,
    )
}

/// Creates multiple test transactions with proper signatures
///
/// # Arguments
/// * `count` - Number of transactions to create
pub fn create_test_transactions(count: usize) -> Vec<Transaction> {
    let mut transactions = Vec::new();
    for i in 0..count {
        let (sk, pk) = gen_tx_keypair();
        let payload = format!("transaction-{}", i).into_bytes();
        transactions.push(create_test_transaction(&sk, &pk, i as u64, payload));
    }
    transactions
}
