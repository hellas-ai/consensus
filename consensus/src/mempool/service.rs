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
//! 3. Pool Management: Route transactions to pending/queued pools based on nonce
//! 4. Proposal Building: Select highest-fee valid transactions for blocks
//! 5. State Validation: Verify balances during proposal building
//! 6. Finalization Cleanup: Remove transactions included in finalized blocks

use super::{
    pool::{AddResult, DEFAULT_POOL_CAPACITY, TransactionPool},
    types::{FinalizedNotification, ProposalRequest, ProposalResponse},
};
use crate::{
    state::{address::Address, transaction::Transaction},
    validation::PendingStateReader,
};
use rtrb::{Consumer, Producer, RingBuffer};
use slog::Logger;
use std::{
    collections::{HashMap, HashSet},
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

/// Constant for transaction batch size.
const TX_BATCH_SIZE: usize = 1024;

/// Mempool service running on a dedicated OS thread.
///
/// The service:
/// - Receives transactions from P2P/RPC via tx_producer
/// - Validates transaction signatures before storing
/// - Routes transactions to pending/queued pools based on nonce
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
#[allow(clippy::too_many_arguments)]
pub fn mempool_loop(
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
    let mut stats_added_pending = 0u64;
    let mut stats_added_queued = 0u64;
    let mut stats_rejected = 0u64;

    slog::info!(logger, "Mempool service started"; "capacity" => pool_capacity);

    while !shutdown.load(Ordering::Acquire) {
        let mut did_work = false;

        // Priority 1: Handle proposal requests (time-critical for consensus)
        while let Ok(req) = proposal_req_consumer.pop() {
            did_work = true;

            let response = build_validated_proposal(&pool, &req, &pending_state_reader, &logger);
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

        // Priority 2: Process incoming transactions
        let available_slots = tx_consumer.slots();
        if available_slots > 0 {
            let num_to_read = available_slots.min(TX_BATCH_SIZE);

            if let Ok(chunk) = tx_consumer.read_chunk(num_to_read) {
                did_work = true;

                for tx in chunk.into_iter() {
                    did_work = true;
                    let tx_hash = tx.tx_hash;
                    let sender = tx.sender;

                    // Verify signature before adding to pool
                    if !tx.verify() {
                        stats_invalid_signatures += 1;
                        slog::debug!(
                            logger,
                            "Transaction rejected: invalid signature";
                            "tx_hash" => hex::encode(&tx_hash[..8]),
                        );
                        continue;
                    }

                    // Get sender's current nonce from chain state
                    let sender_base_nonce = pending_state_reader
                        .get_account(&sender)
                        .map(|a| a.nonce)
                        .unwrap_or(0);

                    // Add to pool with nonce-based routing
                    let result = pool.try_add(Arc::new(tx), sender_base_nonce);

                    match result {
                        AddResult::AddedPending => {
                            stats_added_pending += 1;
                            slog::trace!(
                                logger,
                                "Transaction added to pending pool";
                                "tx_hash" => hex::encode(&tx_hash[..8]),
                                "sender" => hex::encode(&sender.as_bytes()[..8]),
                                "pool_size" => pool.len(),
                            );
                        }
                        AddResult::AddedQueued => {
                            stats_added_queued += 1;
                            slog::trace!(
                                logger,
                                "Transaction added to queued pool (nonce gap)";
                                "tx_hash" => hex::encode(&tx_hash[..8]),
                                "sender" => hex::encode(&sender.as_bytes()[..8]),
                                "pool_size" => pool.len(),
                            );
                        }
                        AddResult::Rejected => {
                            stats_rejected += 1;
                            slog::debug!(
                                logger,
                                "Transaction rejected (duplicate/stale/full)";
                                "tx_hash" => hex::encode(&tx_hash[..8]),
                            );
                        }
                    }
                }
            }
        }

        // Priority 3: Handle finalization notifications
        while let Ok(notif) = finalized_consumer.pop() {
            did_work = true;
            let removed_count = notif.tx_hashes.len();

            // Collect affected senders BEFORE removing transactions
            let mut senders_to_update: HashSet<Address> = notif
                .tx_hashes
                .iter()
                .filter_map(|tx_hash| pool.get(tx_hash).map(|vtx| vtx.tx.sender))
                .collect();

            // Also include senders with queued transactions - they might be promotable
            // (e.g., if their txs were included by another node)
            for sender in pool.queued_senders() {
                senders_to_update.insert(sender);
            }

            // Remove finalized transactions from pool
            pool.remove_finalized(&notif);

            // Update sender nonces based on finalized state
            // This promotes queued transactions that are now executable
            for sender in senders_to_update {
                let new_nonce = pending_state_reader
                    .get_account(&sender)
                    .map(|a| a.nonce)
                    .unwrap_or(0);
                pool.update_sender_nonce(&sender, new_nonce);
            }

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
                "pending" => pool_stats.pending_size,
                "queued" => pool_stats.queued_size,
                "total" => pool_stats.total_size,
                "capacity" => pool_stats.capacity,
                "unique_senders" => pool_stats.unique_senders,
                "total_added" => pool_stats.total_added,
                "total_removed" => pool_stats.total_removed,
                "proposals_built" => stats_proposals_built,
                "added_pending" => stats_added_pending,
                "added_queued" => stats_added_queued,
                "rejected" => stats_rejected,
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
        "final_pending" => pool_stats.pending_size,
        "final_queued" => pool_stats.queued_size,
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
/// With the two-pool design:
/// - `iter_pending()` returns only executable transactions (nonces already valid)
/// - We still validate balances since they can change between insertion and proposal
/// - We track in-block balance changes for multiple txs from the same sender
fn build_validated_proposal(
    pool: &TransactionPool,
    req: &ProposalRequest,
    pending_state_reader: &PendingStateReader,
    logger: &Logger,
) -> ProposalResponse {
    let mut selected = Vec::with_capacity(req.max_txs);
    let mut total_bytes = 0usize;
    let mut total_fees = 0u64;

    // Track in-block balance changes for validation
    // Note: Nonces are already validated by the pending pool, so we only track balances
    let mut pending_balances: HashMap<Address, i128> = HashMap::new();

    // Track which senders we've already included a tx from in this block
    // to ensure we process their nonces sequentially
    let mut sender_next_nonce: HashMap<Address, u64> = HashMap::new();

    let pending_count = pool.pending_len();
    slog::debug!(
        logger,
        "Building proposal";
        "view" => req.view,
        "max_txs" => req.max_txs,
        "max_bytes" => req.max_bytes,
        "pending_pool_size" => pending_count,
    );

    // Iterate transactions in fee-priority order (highest fee first)
    // All transactions from iter_pending() have valid nonces at pool level
    let mut checked_count = 0usize;
    let mut skipped_nonce_gap = 0usize;
    let mut skipped_stale_nonce = 0usize;
    let mut skipped_wrong_nonce = 0usize;
    let mut skipped_insufficient_balance = 0usize;
    let mut skipped_size = 0usize;

    for tx in pool.iter_pending() {
        checked_count += 1;

        if selected.len() >= req.max_txs {
            break;
        }

        let sender = tx.sender;

        // Verify nonce is sequential for this sender within this block
        // This handles the case where we skip a tx (e.g., insufficient balance)
        // and need to also skip subsequent txs from the same sender
        let expected_in_block = sender_next_nonce.get(&sender);
        match expected_in_block {
            Some(&expected) => {
                if tx.nonce != expected {
                    // We already have txs from this sender but there's a gap
                    // (possibly due to skipping a tx with insufficient balance)
                    skipped_nonce_gap += 1;
                    slog::trace!(
                        logger,
                        "Skipping tx: nonce gap in block";
                        "tx_hash" => hex::encode(&tx.tx_hash[..8]),
                        "tx_nonce" => tx.nonce,
                        "expected_nonce" => expected,
                    );
                    continue;
                }
            }
            None => {
                // First tx from sender - validate against chain state to catch stale pool
                let account_exists = pending_state_reader.get_account(&sender).is_some();
                let chain_nonce = pending_state_reader
                    .get_account(&sender)
                    .map(|a| a.nonce)
                    .unwrap_or(0);

                if tx.nonce < chain_nonce {
                    // Stale transaction - already executed on chain
                    skipped_stale_nonce += 1;
                    slog::trace!(
                        logger,
                        "Skipping tx: stale nonce";
                        "tx_hash" => hex::encode(&tx.tx_hash[..8]),
                        "tx_nonce" => tx.nonce,
                        "chain_nonce" => chain_nonce,
                        "account_exists" => account_exists,
                    );
                    continue;
                }
                // Also verify nonce matches exactly (pool might have gaps)
                if tx.nonce != chain_nonce {
                    // Unexpected nonce - skip this sender
                    skipped_wrong_nonce += 1;
                    slog::trace!(
                        logger,
                        "Skipping tx: nonce mismatch";
                        "tx_hash" => hex::encode(&tx.tx_hash[..8]),
                        "tx_nonce" => tx.nonce,
                        "chain_nonce" => chain_nonce,
                        "account_exists" => account_exists,
                    );
                    continue;
                }
            }
        }

        // Get current balance for sender
        let balance = get_effective_balance(&sender, &pending_balances, pending_state_reader);

        // Check sufficient balance for amount + fee
        let required = tx.amount() as i128 + tx.fee as i128;
        if balance < required {
            // Skip this tx, but DON'T update sender_next_nonce
            // This creates a "gap" that will cause subsequent txs to be skipped too
            skipped_insufficient_balance += 1;
            slog::debug!(
                logger,
                "Skipping tx: insufficient balance";
                "tx_hash" => hex::encode(&tx.tx_hash[..8]),
                "sender" => hex::encode(&sender.as_bytes()[..8]),
                "balance" => balance,
                "required" => required,
                "amount" => tx.amount(),
                "fee" => tx.fee,
            );
            continue;
        }

        // Check size constraint
        let tx_size = estimate_tx_size(&tx);
        if total_bytes + tx_size > req.max_bytes {
            skipped_size += 1;
            slog::trace!(
                logger,
                "Skipping tx: size limit";
                "tx_hash" => hex::encode(&tx.tx_hash[..8]),
                "tx_size" => tx_size,
                "total_bytes" => total_bytes,
                "max_bytes" => req.max_bytes,
            );
            continue;
        }

        // Transaction is valid - update in-block state
        *pending_balances.entry(sender).or_insert(balance) -= required;

        // Credit recipient (if applicable)
        if let Some(recipient) = tx.recipient() {
            let recipient_bal =
                get_effective_balance(&recipient, &pending_balances, pending_state_reader);
            *pending_balances.entry(recipient).or_insert(recipient_bal) += tx.amount() as i128;
        }

        // Track the next expected nonce for this sender
        sender_next_nonce.insert(sender, tx.nonce + 1);

        total_bytes += tx_size;
        total_fees += tx.fee;
        selected.push(tx);
    }

    slog::debug!(
        logger,
        "Proposal building complete";
        "view" => req.view,
        "selected" => selected.len(),
        "checked" => checked_count,
        "skipped_nonce_gap" => skipped_nonce_gap,
        "skipped_stale_nonce" => skipped_stale_nonce,
        "skipped_wrong_nonce" => skipped_wrong_nonce,
        "skipped_insufficient_balance" => skipped_insufficient_balance,
        "skipped_size" => skipped_size,
    );

    ProposalResponse {
        view: req.view,
        transactions: selected,
        total_fees,
    }
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
///
/// Components:
/// - Signature: 64 bytes (Ed25519)
/// - Sender: 32 bytes
/// - Nonce: 8 bytes
/// - Fee: 8 bytes
/// - Tx hash: 32 bytes
/// - Instruction type: 1 byte
/// - Instruction data: varies by type
#[inline]
fn estimate_tx_size(tx: &Arc<Transaction>) -> usize {
    const SIGNATURE_SIZE: usize = 64;
    const ADDRESS_SIZE: usize = 32;
    const NONCE_SIZE: usize = 8;
    const FEE_SIZE: usize = 8;
    const HASH_SIZE: usize = 32;
    const INSTRUCTION_TYPE_SIZE: usize = 1;

    let base_size = SIGNATURE_SIZE + ADDRESS_SIZE + NONCE_SIZE + FEE_SIZE + HASH_SIZE;

    // Estimate instruction size based on type
    let instruction_size = match tx.instruction {
        crate::state::transaction::TransactionInstruction::Transfer {
            recipient: _,
            amount: _,
        } => {
            INSTRUCTION_TYPE_SIZE + ADDRESS_SIZE + 8 // recipient (32) + amount (8)
        }
        crate::state::transaction::TransactionInstruction::CreateAccount { address: _ } => {
            INSTRUCTION_TYPE_SIZE + ADDRESS_SIZE // address (32)
        }
        crate::state::transaction::TransactionInstruction::Burn {
            amount: _,
            address: _,
        } => {
            INSTRUCTION_TYPE_SIZE + 8 + ADDRESS_SIZE // amount (8) + address (32)
        }
        crate::state::transaction::TransactionInstruction::Mint {
            recipient: _,
            amount: _,
        } => {
            INSTRUCTION_TYPE_SIZE + ADDRESS_SIZE + 8 // recipient (32) + amount (8)
        }
    };

    base_size + instruction_size
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
        // tx0 goes to pending, tx2 goes to queued
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
        // Only tx with nonce 0 should be included (tx2 is in queued pool)
        assert_eq!(resp.transactions.len(), 1);
        assert_eq!(resp.transactions[0].nonce, 0);

        service.shutdown();
    }

    #[test]
    fn test_queued_promoted_after_gap_filled() {
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

        // Submit transactions out of order (2, 0, 1)
        // This tests that the pool promotes correctly
        let tx2 = create_tx(&sk, sender, recipient, 100, 2, 10);
        let tx0 = create_tx(&sk, sender, recipient, 100, 0, 30);
        let tx1 = create_tx(&sk, sender, recipient, 100, 1, 20);

        channels.tx_producer.push(tx2).unwrap(); // Goes to queued
        channels.tx_producer.push(tx0).unwrap(); // Goes to pending
        channels.tx_producer.push(tx1).unwrap(); // Goes to pending, promotes tx2

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
        // All 3 should be included (tx2 was promoted when tx1 was added)
        assert_eq!(resp.transactions.len(), 3);

        service.shutdown();
    }

    #[test]
    fn test_multi_sender_fee_ordering() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(&path).unwrap());

        let (sk1, sender1) = gen_keypair();
        let (sk2, sender2) = gen_keypair();
        let (_, recipient) = gen_keypair();

        // Fund both senders
        store
            .put_account(&Account::new(sk1.public_key(), 10_000, 0))
            .unwrap();
        store
            .put_account(&Account::new(sk2.public_key(), 10_000, 0))
            .unwrap();

        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let (mut service, mut channels) =
            MempoolService::spawn(reader, Arc::clone(&shutdown), logger);

        // sender1: fee 50, sender2: fee 100
        // sender2 should come first in proposal
        let tx1 = create_tx(&sk1, sender1, recipient, 100, 0, 50);
        let tx2 = create_tx(&sk2, sender2, recipient, 100, 0, 100);

        channels.tx_producer.push(tx1).unwrap();
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
        assert_eq!(resp.transactions.len(), 2);
        // Higher fee tx should come first
        assert_eq!(resp.transactions[0].fee, 100);
        assert_eq!(resp.transactions[1].fee, 50);

        service.shutdown();
    }

    #[test]
    fn test_max_bytes_limit() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(&path).unwrap());

        let (sk, sender) = gen_keypair();
        let (_, recipient) = gen_keypair();
        store
            .put_account(&Account::new(sk.public_key(), 100_000, 0))
            .unwrap();

        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let (mut service, mut channels) =
            MempoolService::spawn(reader, Arc::clone(&shutdown), logger);

        // Submit 10 transactions
        for nonce in 0..10 {
            let tx = create_tx(&sk, sender, recipient, 100, nonce, 10);
            channels.tx_producer.push(tx).unwrap();
        }

        std::thread::sleep(std::time::Duration::from_millis(100));

        // Request with very small max_bytes (only ~1-2 txs should fit)
        // Each tx is ~185 bytes (based on estimate_tx_size)
        let req = ProposalRequest {
            view: 1,
            max_txs: 100,
            max_bytes: 400, // Should fit ~2 transactions
            parent_block_hash: [0u8; 32],
        };
        channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = channels.proposal_resp_consumer.pop().unwrap();
        // Should have fewer than 10 transactions due to size limit
        assert!(resp.transactions.len() < 10);
        assert!(!resp.transactions.is_empty());

        service.shutdown();
    }

    #[test]
    fn test_max_txs_limit() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(&path).unwrap());

        let (sk, sender) = gen_keypair();
        let (_, recipient) = gen_keypair();
        store
            .put_account(&Account::new(sk.public_key(), 100_000, 0))
            .unwrap();

        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let (mut service, mut channels) =
            MempoolService::spawn(reader, Arc::clone(&shutdown), logger);

        // Submit 10 transactions
        for nonce in 0..10 {
            let tx = create_tx(&sk, sender, recipient, 100, nonce, 10);
            channels.tx_producer.push(tx).unwrap();
        }

        std::thread::sleep(std::time::Duration::from_millis(100));

        // Request with max_txs = 3
        let req = ProposalRequest {
            view: 1,
            max_txs: 3,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = channels.proposal_resp_consumer.pop().unwrap();
        assert_eq!(resp.transactions.len(), 3);
        // Should be nonces 0, 1, 2
        assert_eq!(resp.transactions[0].nonce, 0);
        assert_eq!(resp.transactions[1].nonce, 1);
        assert_eq!(resp.transactions[2].nonce, 2);

        service.shutdown();
    }

    #[test]
    fn test_balance_exhaustion_across_multiple_txs() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(&path).unwrap());

        let (sk, sender) = gen_keypair();
        let (_, recipient) = gen_keypair();
        // Balance of 250: can afford 2 txs @ 100 + 10 fee = 110 each (220 total)
        // Third tx would need 330 total, which exceeds 250
        store
            .put_account(&Account::new(sk.public_key(), 250, 0))
            .unwrap();

        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let (mut service, mut channels) =
            MempoolService::spawn(reader, Arc::clone(&shutdown), logger);

        // Submit 3 transactions, each costs 110 (100 amount + 10 fee)
        for nonce in 0..3 {
            let tx = create_tx(&sk, sender, recipient, 100, nonce, 10);
            channels.tx_producer.push(tx).unwrap();
        }

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
        // Only 2 txs should be included (balance exhausted after 2)
        assert_eq!(resp.transactions.len(), 2);
        assert_eq!(resp.transactions[0].nonce, 0);
        assert_eq!(resp.transactions[1].nonce, 1);

        service.shutdown();
    }

    #[test]
    fn test_skip_subsequent_after_insufficient_balance() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(&path).unwrap());

        let (sk, sender) = gen_keypair();
        let (_, recipient) = gen_keypair();
        // Balance of 50: tx0 needs 510, tx1 needs 60
        store
            .put_account(&Account::new(sk.public_key(), 50, 0))
            .unwrap();

        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let (mut service, mut channels) =
            MempoolService::spawn(reader, Arc::clone(&shutdown), logger);

        // tx0: amount 500, fee 10 -> needs 510 (can't afford)
        // tx1: amount 50, fee 10 -> needs 60 (could afford if tx0 didn't exist)
        let tx0 = create_tx(&sk, sender, recipient, 500, 0, 10);
        let tx1 = create_tx(&sk, sender, recipient, 50, 1, 10);

        channels.tx_producer.push(tx0).unwrap();
        channels.tx_producer.push(tx1).unwrap();

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
        // Neither tx should be included:
        // - tx0 fails balance check
        // - tx1 is skipped because tx0 created a nonce gap
        assert!(resp.transactions.is_empty());

        service.shutdown();
    }

    #[test]
    fn test_finalization_removes_transactions() {
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

        // Submit 5 transactions
        let mut tx_hashes = Vec::new();
        for nonce in 0..5 {
            let tx = create_tx(&sk, sender, recipient, 100, nonce, 10);
            tx_hashes.push(tx.tx_hash);
            channels.tx_producer.push(tx).unwrap();
        }

        std::thread::sleep(std::time::Duration::from_millis(100));

        // First proposal should have all 5
        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        channels.proposal_req_producer.push(req).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = channels.proposal_resp_consumer.pop().unwrap();
        assert_eq!(resp.transactions.len(), 5);

        // Finalize first 2 transactions
        let finalized = FinalizedNotification {
            view: 1,
            tx_hashes: vec![tx_hashes[0], tx_hashes[1]],
        };
        channels.finalized_producer.push(finalized).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        // Update sender nonce in store to simulate chain state update
        store
            .put_account(&Account::new(sk.public_key(), 9780, 2)) // 10000 - 2*(100+10)
            .unwrap();

        // Second proposal should have remaining 3
        let req2 = ProposalRequest {
            view: 2,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        channels.proposal_req_producer.push(req2).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp2 = channels.proposal_resp_consumer.pop().unwrap();
        assert_eq!(resp2.transactions.len(), 3);
        // Should be nonces 2, 3, 4
        assert_eq!(resp2.transactions[0].nonce, 2);
        assert_eq!(resp2.transactions[1].nonce, 3);
        assert_eq!(resp2.transactions[2].nonce, 4);

        service.shutdown();
    }

    #[test]
    fn test_stale_nonce_excluded() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(&path).unwrap());

        let (sk, sender) = gen_keypair();
        let (_, recipient) = gen_keypair();
        // Start with nonce 0
        store
            .put_account(&Account::new(sk.public_key(), 10_000, 0))
            .unwrap();

        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let (mut service, mut channels) =
            MempoolService::spawn(reader, Arc::clone(&shutdown), logger);

        // Submit transactions with nonces 0, 1, 2
        for nonce in 0..3 {
            let tx = create_tx(&sk, sender, recipient, 100, nonce, 10);
            channels.tx_producer.push(tx).unwrap();
        }

        std::thread::sleep(std::time::Duration::from_millis(100));

        // Simulate chain advancing: update nonce to 2 (nonces 0, 1 already executed)
        store
            .put_account(&Account::new(sk.public_key(), 9780, 2))
            .unwrap();

        // Request proposal - pool still has stale txs but they should be filtered
        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = channels.proposal_resp_consumer.pop().unwrap();
        // Only nonce 2 should be included (0 and 1 are stale)
        assert_eq!(resp.transactions.len(), 1);
        assert_eq!(resp.transactions[0].nonce, 2);

        service.shutdown();
    }

    #[test]
    fn test_empty_proposal_on_empty_pool() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(&path).unwrap());
        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let (mut service, mut channels) =
            MempoolService::spawn(reader, Arc::clone(&shutdown), logger);

        // Request proposal without submitting any transactions
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
        assert_eq!(resp.total_fees, 0);

        service.shutdown();
    }

    #[test]
    fn test_total_fees_calculated_correctly() {
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

        // Submit transactions with fees 10, 20, 30
        for (nonce, fee) in [(0, 10), (1, 20), (2, 30)] {
            let tx = create_tx(&sk, sender, recipient, 100, nonce, fee);
            channels.tx_producer.push(tx).unwrap();
        }

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
        assert_eq!(resp.transactions.len(), 3);
        assert_eq!(resp.total_fees, 60); // 10 + 20 + 30

        service.shutdown();
    }

    #[test]
    fn test_recipient_balance_credited() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(&path).unwrap());

        let (sk1, sender1) = gen_keypair();
        let (sk2, sender2) = gen_keypair();

        // sender1 sends to sender2, then sender2 uses received funds
        store
            .put_account(&Account::new(sk1.public_key(), 1000, 0))
            .unwrap();
        store
            .put_account(&Account::new(sk2.public_key(), 50, 0)) // Only 50 initially
            .unwrap();

        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let (mut service, mut channels) =
            MempoolService::spawn(reader, Arc::clone(&shutdown), logger);

        // sender1 sends 500 to sender2 (fee 10)
        let tx1 = create_tx(&sk1, sender1, sender2, 500, 0, 10);
        // sender2 tries to send 400 (fee 10) - needs the 500 from tx1
        let tx2 = create_tx(&sk2, sender2, sender1, 400, 0, 10);

        channels.tx_producer.push(tx1).unwrap();
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
        // Both transactions should be included:
        // - tx1: sender1 (1000) sends 500+10 to sender2
        // - tx2: sender2 (50 + 500 received = 550) sends 400+10
        assert_eq!(resp.transactions.len(), 2);

        service.shutdown();
    }
}
