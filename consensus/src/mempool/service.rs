//! Mempool Service - Thread Management
//!
//! Spawns a dedicated OS thread for mempool operations to avoid blocking
//! the consensus engine. Uses lock-free rtrb channels for communication.
//!
//! ## Architecture
//!
//!
//! P2P/RPC ──[tx_channel]──► Mempool ◄──[proposal_req_channel]── Consensus
//!                              │
//!                              ├──[proposal_resp_channel]──► Consensus
//!                              │
//!                              ◄──[finalized_channel]────── Consensus
//!
//! ## Responsibilities
//!
//! 1. Transaction Ingestion: Receive transactions from P2P/RPC
//! 2. Signature Verification: Validate Ed25519 signatures before storing
//! 3. Proposal Building: Select highest-fee valid transactions for blocks
//! 4. State Validation: Verify nonces and balances during proposal building
//! 5. Finalization Cleanup: Remove transactions included in finalized blocks

use super::{
    pool::{DEFAULT_POOL_CAPACITY, TransactionPool},
    types::{FinalizedNotification, ProposalRequest, ProposalResponse},
};
use crate::{
    state::{address::Address, transaction::Transaction},
    validation::PendingStateReader,
};
use rtrb::{Consumer, Producer, RingBuffer};
use slog::Logger;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

/// Default ring buffer size for channels.
const RING_BUFFER_SIZE: usize = 256;

/// Ring buffer size for transaction channel (larger due to higher volume).
const TX_RING_BUFFER_SIZE: usize = 1024;

/// Number of transactions to process per loop iteration (quota).
const TX_PROCESS_QUOTA: usize = 64;

/// Mempool service running on a dedicated OS thread.
///
/// The service:
/// - Receives transactions from P2P/RPC via tx_producer
/// - Validates transaction signatures before storing
/// - Builds block proposals with state-validated transactions
/// - Removes finalized transactions from the pool
pub struct MempoolService {
    handle: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

/// Channel endpoints for communicating with the mempool service.
pub struct MempoolChannels {
    /// Producer for submitting transactions (P2P/RPC → Mempool)
    pub tx_producer: Producer<Transaction>,
    /// Consumer for receiving block proposals (Mempool → Consensus)
    pub proposal_resp_consumer: Consumer<ProposalResponse>,
    /// Producer for requesting block proposals (Consensus → Mempool)
    pub proposal_req_producer: Producer<ProposalRequest>,
    /// Producer for notifying about finalized blocks (Consensus → Mempool)
    pub finalized_producer: Producer<FinalizedNotification>,
}

impl MempoolService {
    /// Spawns the mempool service on a new OS thread.
    ///
    /// # Arguments
    ///
    /// * pending_state_reader - Reader for M-notarized pending state
    /// * shutdown - Shared shutdown signal
    /// * logger - Logger for diagnostics
    ///
    /// # Returns
    ///
    /// A tuple containing the service handle and channel endpoints.
    pub fn spawn(
        pending_state_reader: PendingStateReader,
        shutdown: Arc<AtomicBool>,
        logger: Logger,
    ) -> (Self, MempoolChannels) {
        Self::spawn_with_capacity(
            DEFAULT_POOL_CAPACITY,
            pending_state_reader,
            shutdown,
            logger,
        )
    }

    /// Spawns the mempool service with custom pool capacity.
    pub fn spawn_with_capacity(
        pool_capacity: usize,
        pending_state_reader: PendingStateReader,
        shutdown: Arc<AtomicBool>,
        logger: Logger,
    ) -> (Self, MempoolChannels) {
        // Transaction input channel (P2P/RPC → Mempool)
        let (tx_producer, tx_consumer) = RingBuffer::<Transaction>::new(TX_RING_BUFFER_SIZE);
        // Proposal request channel (Consensus → Mempool)
        let (proposal_req_producer, proposal_req_consumer) =
            RingBuffer::<ProposalRequest>::new(RING_BUFFER_SIZE);
        // Proposal response channel (Mempool → Consensus)
        let (proposal_resp_producer, proposal_resp_consumer) =
            RingBuffer::<ProposalResponse>::new(RING_BUFFER_SIZE);
        // Finalization notification channel (Consensus → Mempool)
        let (finalized_producer, finalized_consumer) =
            RingBuffer::<FinalizedNotification>::new(RING_BUFFER_SIZE);
        let shutdown_clone = Arc::clone(&shutdown);
        let logger_clone = logger.clone();
        let handle = thread::Builder::new()
            .name("mempool".into())
            .spawn(move || {
                mempool_loop(
                    pool_capacity,
                    pending_state_reader,
                    tx_consumer,
                    proposal_req_consumer,
                    proposal_resp_producer,
                    finalized_consumer,
                    shutdown_clone,
                    logger_clone,
                );
            })
            .expect("Failed to spawn mempool thread");
        let channels = MempoolChannels {
            tx_producer,
            proposal_resp_consumer,
            proposal_req_producer,
            finalized_producer,
        };
        (
            Self {
                handle: Some(handle),
                shutdown,
            },
            channels,
        )
    }

    /// Signals shutdown and waits for the thread to terminate.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Returns true if the service is still running.
    pub fn is_running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }
}

impl Drop for MempoolService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Main mempool event loop.
fn mempool_loop(
    pool_capacity: usize,
    pending_state_reader: PendingStateReader,
    mut tx_consumer: Consumer<Transaction>,
    mut proposal_req_consumer: Consumer<ProposalRequest>,
    mut proposal_resp_producer: Producer<ProposalResponse>,
    mut finalized_consumer: Consumer<FinalizedNotification>,
    shutdown: Arc<AtomicBool>,
    logger: Logger,
) {
    let mut pool = TransactionPool::new(pool_capacity);
    let mut idle_count = 0_u32;
    let mut stats_interval = std::time::Instant::now();

    // Statistics
    let mut stats_proposals_built = 0u64;
    let mut stats_invalid_signatures = 0u64;

    slog::info!(logger, "Mempool service started"; "capacity" => pool_capacity);

    while !shutdown.load(Ordering::Acquire) {
        let mut did_work = false;

        // Priority 1: Handle proposal requests (time-critical for consensus)
        while let Ok(req) = proposal_req_consumer.pop() {
            did_work = true;

            let response = build_validated_proposal(&pool, &req, &pending_state_reader);
            let tx_count = response.transactions.len();
            let total_fees = response.total_fees;

            // Push with backpressure handling
            push_with_backpressure(&mut proposal_resp_producer, response, &shutdown);
            stats_proposals_built += 1;

            slog::debug!(
            logger,
            "Built block proposal";
            "view" => req.view,
            "tx_count" => tx_count,
            "total_fees" => total_fees,
            "pool_size" => pool.len(),
            );
        }

        // Priority 2: Process incoming transactions (quota-limited)
        for _ in 0..TX_PROCESS_QUOTA {
            match tx_consumer.pop() {
                Ok(tx) => {
                    did_work = true;
                    let tx_hash = tx.tx_hash;
                    // Verify signature before adding to pool
                    if tx.verify() {
                        if pool.try_add(Arc::new(tx)) {
                            slog::trace!(
                            logger,
                            "Transaction added to pool";
                            "tx_hash" => hex::encode(&tx_hash[..8]),
                            "pool_size" => pool.len(),
                            );
                        }
                    } else {
                        stats_invalid_signatures += 1;
                        slog::debug!(
                        logger,
                        "Transaction rejected: invalid signature";
                        "tx_hash" => hex::encode(&tx_hash[..8]),
                        );
                    }
                }
                Err(_) => break,
            }
        }

        // Priority 3: Handle finalization notifications
        while let Ok(notif) = finalized_consumer.pop() {
            did_work = true;
            let removed_count = notif.tx_hashes.len();
            pool.remove_finalized(&notif);
            slog::debug!(
            logger,
            "Removed finalized transactions";
            "view" => notif.view,
            "removed_count" => removed_count,
            "pool_size" => pool.len(),
            );
        }

        // Periodic stats logging
        if stats_interval.elapsed() >= std::time::Duration::from_secs(30) {
            let pool_stats = pool.stats();
            slog::info!(
            logger,
            "Mempool stats";
            "size" => pool_stats.current_size,
            "capacity" => pool_stats.capacity,
            "unique_senders" => pool_stats.unique_senders,
            "total_added" => pool_stats.total_added,
            "total_removed" => pool_stats.total_removed,
            "proposals_built" => stats_proposals_built,
            "invalid_signatures" => stats_invalid_signatures,
            );
            stats_interval = std::time::Instant::now();
        }

        // Progressive backoff when idle
        if did_work {
            idle_count = 0;
        } else {
            idle_count = idle_count.saturating_add(1);
            if idle_count < 10 {
                std::hint::spin_loop();
            } else if idle_count < 100 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }

    let pool_stats = pool.stats();

    slog::info!(
        logger,
        "Mempool service shutting down";
        "final_size" => pool_stats.current_size,
        "total_added" => pool_stats.total_added,
        "total_removed" => pool_stats.total_removed,
        "proposals_built" => stats_proposals_built,
    );
}

/// Push a response with backpressure handling.
fn push_with_backpressure(
    producer: &mut Producer<ProposalResponse>,
    response: ProposalResponse,
    shutdown: &Arc<AtomicBool>,
) {
    let mut resp = response;
    loop {
        match producer.push(resp) {
            Ok(()) => break,
            Err(rtrb::PushError::Full(returned)) => {
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                std::thread::yield_now();
                resp = returned;
            }
        }
    }
}

/// Builds a block proposal with state-validated transactions.
///
/// This function:
/// 1. Iterates transactions in fee-priority order
/// 2. Validates each transaction against current state (nonces, balances)
/// 3. Tracks in-proposal state changes to handle multiple txs from same sender
/// 4. Returns only transactions that form a coherent, valid block
fn build_validated_proposal(
    pool: &TransactionPool,
    req: &ProposalRequest,
    pending_state_reader: &PendingStateReader,
) -> ProposalResponse {
    let mut selected = Vec::with_capacity(req.max_txs);
    let mut total_bytes = 0usize;
    let mut total_fees = 0u64;

    // Track in-block state changes for validation
    let mut pending_nonces: HashMap<Address, u64> = HashMap::new();
    let mut pending_balances: HashMap<Address, i128> = HashMap::new();

    // Iterate transactions in priority order (highest fee first)
    for tx in pool.iter_by_priority() {
        if selected.len() >= req.max_txs {
            break;
        }

        // Get current nonce for sender
        let expected_nonce = get_effective_nonce(&tx.sender, &pending_nonces, pending_state_reader);

        // Skip if nonce doesn't match (gap or already used)
        if tx.nonce != expected_nonce {
            continue;
        }

        // Get current balance for sender
        let balance = get_effective_balance(&tx.sender, &pending_balances, pending_state_reader);

        // Check sufficient balance for amount + fee
        let required = tx.amount() as i128 + tx.fee as i128;
        if balance < required {
            continue;
        }

        // Check size constraint
        let tx_size = estimate_tx_size(&tx);
        if total_bytes + tx_size > req.max_bytes {
            continue;
        }

        // Transaction is valid - update in-block state
        pending_nonces.insert(tx.sender, expected_nonce + 1);
        *pending_balances.entry(tx.sender).or_insert(balance) -= required;

        // Credit recipient (if applicable)
        if let Some(recipient) = tx.recipient() {
            let recipient_bal =
                get_effective_balance(&recipient, &pending_balances, pending_state_reader);
            *pending_balances.entry(recipient).or_insert(recipient_bal) += tx.amount() as i128;
        }

        total_bytes += tx_size;
        total_fees += tx.fee;
        selected.push(tx);
    }

    ProposalResponse {
        view: req.view,
        transactions: selected,
        total_fees,
    }
}

/// Gets the effective nonce for an address.
///
/// Priority: in-block pending > pending state > 0
fn get_effective_nonce(
    address: &Address,
    pending_nonces: &HashMap<Address, u64>,
    pending_state_reader: &PendingStateReader,
) -> u64 {
    // Check in-block pending state first
    if let Some(&nonce) = pending_nonces.get(address) {
        return nonce;
    }

    // Check pending state (M-notarized blocks)
    if let Some(account_state) = pending_state_reader.get_account(address) {
        return account_state.nonce;
    }

    // Account doesn't exist
    0
}

/// Gets the effective balance for an address.
///
/// Priority: in-block pending > pending state > 0
fn get_effective_balance(
    address: &Address,
    pending_balances: &HashMap<Address, i128>,
    pending_state_reader: &PendingStateReader,
) -> i128 {
    // Check in-block pending state first
    if let Some(&balance) = pending_balances.get(address) {
        return balance;
    }

    // Check pending state (M-notarized blocks)
    if let Some(account_state) = pending_state_reader.get_account(address) {
        return account_state.balance as i128;
    }

    // Account doesn't exist
    0
}

/// Estimates the serialized size of a transaction.
#[inline]
fn estimate_tx_size(_tx: &Arc<Transaction>) -> usize {
    const BASE_SIZE: usize = 153;
    const INSTRUCTION_SIZE: usize = 40;
    BASE_SIZE + INSTRUCTION_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::transaction_crypto::TxSecretKey;
    use crate::state::account::Account;
    use crate::storage::store::ConsensusStore;
    use crate::validation::PendingStateWriter;
    use std::path::PathBuf;

    fn temp_db_path() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "mempool_service_test_{}.redb",
            rand::random::<u64>()
        ));
        p
    }

    fn gen_keypair() -> (TxSecretKey, Address) {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let addr = Address::from_public_key(&pk);
        (sk, addr)
    }

    fn create_tx(
        sender_sk: &TxSecretKey,
        sender: Address,
        recipient: Address,
        amount: u64,
        nonce: u64,
        fee: u64,
    ) -> Transaction {
        Transaction::new_transfer(sender, recipient, amount, nonce, fee, sender_sk)
    }

    #[test]
    fn test_service_starts_and_stops() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(&path).unwrap());
        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let (mut service, _channels) = MempoolService::spawn(reader, Arc::clone(&shutdown), logger);

        assert!(service.is_running());

        service.shutdown();

        assert!(service.handle.is_none());
    }

    #[test]
    fn test_transaction_submission_and_proposal() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(&path).unwrap());

        // Create and fund sender account
        let (sk, sender) = gen_keypair();
        let (_, recipient) = gen_keypair();
        store
            .put_account(&Account::new(sk.public_key(), 10_000, 0))
            .unwrap();

        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let (mut service, mut channels) =
            MempoolService::spawn(reader, Arc::clone(&shutdown), logger);

        // Submit transactions with sequential nonces
        for nonce in 0..5 {
            let tx = create_tx(&sk, sender, recipient, 100, nonce, 10 + nonce);
            channels.tx_producer.push(tx).unwrap();
        }

        // Wait for processing
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Request proposal
        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        channels.proposal_req_producer.push(req).unwrap();

        // Wait for response
        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = channels.proposal_resp_consumer.pop().unwrap();
        assert_eq!(resp.view, 1);
        assert_eq!(resp.transactions.len(), 5);

        service.shutdown();
    }

    #[test]
    fn test_invalid_signature_rejected() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(&path).unwrap());
        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let (mut service, mut channels) =
            MempoolService::spawn(reader, Arc::clone(&shutdown), logger);

        let (sk, _sender) = gen_keypair();
        let (_, wrong_sender) = gen_keypair();
        let (_, recipient) = gen_keypair();

        // Create tx with mismatched sender/signature
        let invalid_tx = create_tx(&sk, wrong_sender, recipient, 100, 0, 10);
        channels.tx_producer.push(invalid_tx).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = channels.proposal_resp_consumer.pop().unwrap();
        assert!(resp.transactions.is_empty());

        service.shutdown();
    }

    #[test]
    fn test_insufficient_balance_excluded() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(&path).unwrap());

        let (sk, sender) = gen_keypair();
        let (_, recipient) = gen_keypair();
        store
            .put_account(&Account::new(sk.public_key(), 100, 0))
            .unwrap();

        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let (mut service, mut channels) =
            MempoolService::spawn(reader, Arc::clone(&shutdown), logger);

        // Submit transaction that exceeds balance (needs 210, has 100)
        let tx = create_tx(&sk, sender, recipient, 200, 0, 10);
        channels.tx_producer.push(tx).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = channels.proposal_resp_consumer.pop().unwrap();
        assert!(resp.transactions.is_empty());

        service.shutdown();
    }

    #[test]
    fn test_nonce_gap_excluded() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(&path).unwrap());

        let (sk, sender) = gen_keypair();
        let (_, recipient) = gen_keypair();
        store
            .put_account(&Account::new(sk.public_key(), 10_000, 0))
            .unwrap();

        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let (mut service, mut channels) =
            MempoolService::spawn(reader, Arc::clone(&shutdown), logger);

        // Submit transactions with nonce gap (0, 2 - missing 1)
        let tx0 = create_tx(&sk, sender, recipient, 100, 0, 20);
        let tx2 = create_tx(&sk, sender, recipient, 100, 2, 10);

        channels.tx_producer.push(tx0).unwrap();
        channels.tx_producer.push(tx2).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = channels.proposal_resp_consumer.pop().unwrap();
        // Only tx with nonce 0 should be included
        assert_eq!(resp.transactions.len(), 1);
        assert_eq!(resp.transactions[0].nonce, 0);

        service.shutdown();
    }
}
