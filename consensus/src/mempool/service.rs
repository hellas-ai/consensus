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
    pool::{AddResult, DEFAULT_POOL_CAPACITY, PoolStats, TransactionPool},
    types::{FinalizedNotification, ProposalRequest, ProposalResponse},
};
use crate::{
    state::{address::Address, transaction::Transaction},
    validation::PendingStateReader,
};
use arc_swap::ArcSwap;
use crossbeam::queue::ArrayQueue;
use crossbeam::sync::{Parker, Unparker};
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
    /// Unparker to wake up the mempool thread immediately on shutdown.
    /// Similar to P2P's `shutdown_notify`, this ensures the thread doesn't
    /// sleep through the shutdown signal during idle backoff.
    shutdown_unparker: Unparker,
}

/// Channel endpoints for communicating with the mempool service.
pub struct MempoolChannels {
    /// Consumer for receiving block proposals (Mempool → Consensus)
    pub proposal_resp_consumer: Consumer<ProposalResponse>,
    /// Producer for requesting block proposals (Consensus → Mempool)
    pub proposal_req_producer: Producer<ProposalRequest>,
    /// Producer for notifying about finalized blocks (Consensus → Mempool)
    pub finalized_producer: Producer<FinalizedNotification>,
    /// Lock-free reader for mempool statistics (updated periodically by mempool thread)
    pub stats_reader: MempoolStatsReader,
}

/// Lock-free reader for mempool statistics.
///
/// Uses ArcSwap for wait-free reads from the gRPC layer while the
/// mempool thread periodically updates the stats.
#[derive(Clone)]
pub struct MempoolStatsReader {
    inner: Arc<ArcSwap<PoolStats>>,
}

impl MempoolStatsReader {
    /// Load the current stats snapshot.
    pub fn load(&self) -> arc_swap::Guard<Arc<PoolStats>> {
        self.inner.load()
    }
}

impl MempoolService {
    /// Spawns the mempool service on a new OS thread.
    ///
    /// # Arguments
    ///
    /// * `grpc_tx_queue` - Lock-free queue for transactions from gRPC clients. This is an
    ///   `Arc<ArrayQueue>` which is `Sync`, allowing multiple gRPC handlers to push transactions
    ///   concurrently without a Mutex.
    /// * `p2p_tx_consumer` - SPSC consumer for transactions received via P2P gossip. Uses rtrb for
    ///   efficient single-producer (P2P thread) to single-consumer (mempool thread) communication.
    /// * `pending_state_reader` - Reader for M-notarized pending state
    /// * `shutdown` - Shared shutdown signal
    /// * `logger` - Logger for diagnostics
    ///
    /// # Returns
    ///
    /// A tuple containing the service handle and channel endpoints.
    pub fn spawn(
        grpc_tx_queue: Arc<ArrayQueue<Transaction>>,
        p2p_tx_consumer: Consumer<Transaction>,
        pending_state_reader: PendingStateReader,
        shutdown: Arc<AtomicBool>,
        logger: Logger,
    ) -> (Self, MempoolChannels) {
        Self::spawn_with_capacity(
            DEFAULT_POOL_CAPACITY,
            grpc_tx_queue,
            p2p_tx_consumer,
            pending_state_reader,
            shutdown,
            logger,
        )
    }

    /// Spawns the mempool service with custom pool capacity.
    pub fn spawn_with_capacity(
        pool_capacity: usize,
        grpc_tx_queue: Arc<ArrayQueue<Transaction>>,
        p2p_tx_consumer: Consumer<Transaction>,
        pending_state_reader: PendingStateReader,
        shutdown: Arc<AtomicBool>,
        logger: Logger,
    ) -> (Self, MempoolChannels) {
        // Proposal request channel (Consensus → Mempool)
        let (proposal_req_producer, proposal_req_consumer) =
            RingBuffer::<ProposalRequest>::new(RING_BUFFER_SIZE);
        // Proposal response channel (Mempool → Consensus)
        let (proposal_resp_producer, proposal_resp_consumer) =
            RingBuffer::<ProposalResponse>::new(RING_BUFFER_SIZE);
        // Finalization notification channel (Consensus → Mempool)
        let (finalized_producer, finalized_consumer) =
            RingBuffer::<FinalizedNotification>::new(RING_BUFFER_SIZE);

        // Shared stats for lock-free reads from gRPC
        let stats_shared = Arc::new(ArcSwap::from_pointee(PoolStats::default()));
        let stats_writer = Arc::clone(&stats_shared);
        let stats_reader = MempoolStatsReader {
            inner: stats_shared,
        };

        // Create parker/unparker pair for instant shutdown wake-up
        // Similar to P2P's shutdown_notify pattern, but for OS threads
        let parker = Parker::new();
        let shutdown_unparker = parker.unparker().clone();

        let shutdown_clone = Arc::clone(&shutdown);
        let logger_clone = logger.clone();

        let handle = thread::Builder::new()
            .name("mempool".into())
            .spawn(move || {
                mempool_loop(
                    pool_capacity,
                    pending_state_reader,
                    grpc_tx_queue,
                    p2p_tx_consumer,
                    proposal_req_consumer,
                    proposal_resp_producer,
                    finalized_consumer,
                    stats_writer,
                    shutdown_clone,
                    parker,
                    logger_clone,
                );
            })
            .expect("Failed to spawn mempool thread");

        let channels = MempoolChannels {
            proposal_resp_consumer,
            proposal_req_producer,
            finalized_producer,
            stats_reader,
        };

        (
            Self {
                handle: Some(handle),
                shutdown,
                shutdown_unparker,
            },
            channels,
        )
    }

    /// Signals shutdown and waits for the thread to terminate.
    ///
    /// This method:
    /// 1. Sets the shutdown flag (checked by mempool_loop)
    /// 2. Wakes up the thread immediately via unpark (no waiting for idle timeout)
    /// 3. Joins the thread to ensure clean shutdown
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        // Wake up the thread immediately if it's parked during idle backoff
        self.shutdown_unparker.unpark();
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

/// Process a single incoming transaction.
///
/// Validates the signature and adds to the pool with proper nonce-based routing.
/// Updates statistics counters based on the result.
#[allow(clippy::too_many_arguments)]
fn process_incoming_tx(
    tx: Transaction,
    pool: &mut TransactionPool,
    pending_state_reader: &PendingStateReader,
    stats_invalid_signatures: &mut u64,
    stats_added_pending: &mut u64,
    stats_added_queued: &mut u64,
    stats_rejected: &mut u64,
    logger: &Logger,
) {
    let tx_hash = tx.tx_hash;
    let sender = tx.sender;
    let tx_nonce = tx.nonce;

    // Verify signature before adding to pool
    if !tx.verify() {
        *stats_invalid_signatures += 1;
        slog::debug!(
            logger,
            "Transaction rejected: invalid signature";
            "tx_hash" => hex::encode(&tx_hash[..8]),
        );
        return;
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
            *stats_added_pending += 1;
            slog::debug!(
                logger,
                "Transaction added to PENDING pool";
                "tx_hash" => hex::encode(&tx_hash[..8]),
                "sender" => hex::encode(&sender.as_bytes()[..8]),
                "sender_base_nonce" => sender_base_nonce,
                "pool_size" => pool.len(),
            );
        }
        AddResult::AddedQueued => {
            *stats_added_queued += 1;
            slog::debug!(
                logger,
                "Transaction added to QUEUED pool (nonce gap)";
                "tx_hash" => hex::encode(&tx_hash[..8]),
                "sender" => hex::encode(&sender.as_bytes()[..8]),
                "sender_base_nonce" => sender_base_nonce,
                "tx_nonce" => tx_nonce,
                "pool_size" => pool.len(),
            );
        }
        AddResult::Rejected => {
            *stats_rejected += 1;
            slog::debug!(
                logger,
                "Transaction rejected (duplicate/stale/full)";
                "tx_hash" => hex::encode(&tx_hash[..8]),
            );
        }
    }
}

/// Main mempool event loop.
///
/// Receives transactions from two sources:
/// - `grpc_tx_queue`: Lock-free ArrayQueue for gRPC-submitted transactions (MPSC)
/// - `p2p_tx_consumer`: rtrb Consumer for P2P-gossipped transactions (SPSC)
///
/// # Shutdown Behavior
///
/// When shutdown is signaled:
/// 1. The parker is unparked immediately (no waiting for idle timeout)
/// 2. The loop exits on the next iteration
/// 3. Any pending proposal requests are handled before exit
#[allow(clippy::too_many_arguments)]
pub fn mempool_loop(
    pool_capacity: usize,
    pending_state_reader: PendingStateReader,
    grpc_tx_queue: Arc<ArrayQueue<Transaction>>,
    mut p2p_tx_consumer: Consumer<Transaction>,
    mut proposal_req_consumer: Consumer<ProposalRequest>,
    mut proposal_resp_producer: Producer<ProposalResponse>,
    mut finalized_consumer: Consumer<FinalizedNotification>,
    stats_writer: Arc<ArcSwap<PoolStats>>,
    shutdown: Arc<AtomicBool>,
    parker: Parker,
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

        // Priority 2: Process incoming transactions from BOTH sources
        // - grpc_tx_queue: ArrayQueue from gRPC clients (MPSC)
        // - p2p_tx_consumer: rtrb from P2P gossip (SPSC)
        // Pop up to TX_BATCH_SIZE transactions per iteration total
        let mut tx_count = 0;

        // Process gRPC transactions first
        while tx_count < TX_BATCH_SIZE {
            if let Some(tx) = grpc_tx_queue.pop() {
                did_work = true;
                tx_count += 1;
                process_incoming_tx(
                    tx,
                    &mut pool,
                    &pending_state_reader,
                    &mut stats_invalid_signatures,
                    &mut stats_added_pending,
                    &mut stats_added_queued,
                    &mut stats_rejected,
                    &logger,
                );
            } else {
                break;
            }
        }

        // Process P2P gossipped transactions
        while tx_count < TX_BATCH_SIZE {
            if let Ok(tx) = p2p_tx_consumer.pop() {
                did_work = true;
                tx_count += 1;
                process_incoming_tx(
                    tx,
                    &mut pool,
                    &pending_state_reader,
                    &mut stats_invalid_signatures,
                    &mut stats_added_pending,
                    &mut stats_added_queued,
                    &mut stats_rejected,
                    &logger,
                );
            } else {
                break;
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

            slog::debug!(
                logger,
                "Processing finalization";
                "view" => notif.view,
                "finalized_tx_count" => removed_count,
                "senders_to_update" => senders_to_update.len(),
            );

            // Update sender nonces based on finalized state
            // This promotes queued transactions that are now executable
            for sender in senders_to_update {
                let new_nonce = pending_state_reader
                    .get_account(&sender)
                    .map(|a| a.nonce)
                    .unwrap_or(0);
                slog::debug!(
                    logger,
                    "Updating sender nonce after finalization";
                    "sender" => hex::encode(&sender.as_bytes()[..8]),
                    "new_nonce" => new_nonce,
                );
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

        // Periodic stats logging and publishing
        if stats_interval.elapsed() >= std::time::Duration::from_secs(30) {
            let pool_stats = pool.stats();

            // Publish to gRPC layer (lock-free write)
            stats_writer.store(Arc::new(pool_stats.clone()));

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
        // Uses parker.park_timeout() instead of thread::sleep() so that
        // shutdown can wake us immediately via unpark()
        if did_work {
            idle_count = 0;
        } else {
            idle_count = idle_count.saturating_add(1);
            if idle_count < 10 {
                std::hint::spin_loop();
            } else if idle_count < 100 {
                std::thread::yield_now();
            } else {
                // Park with timeout - can be woken immediately by shutdown_unparker.unpark()
                parker.park_timeout(std::time::Duration::from_millis(10));
            }
        }
    }

    // Handle any pending proposal requests before exiting.
    // This ensures consensus gets responses for in-flight requests.
    slog::info!(
        logger,
        "Mempool shutting down, handling final proposal requests..."
    );

    let mut final_proposals = 0u64;
    while let Ok(req) = proposal_req_consumer.pop() {
        let response = build_validated_proposal(&pool, &req, &pending_state_reader, &logger);
        // Best-effort push - don't block if consensus is already gone
        if proposal_resp_producer.push(response).is_ok() {
            final_proposals += 1;
        }
    }

    let pool_stats = pool.stats();

    slog::info!(
        logger,
        "Mempool service shutdown complete";
        "final_proposals_handled" => final_proposals,
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

    const TEST_BUFFER_SIZE: usize = 1024;

    /// Test helper that bundles mempool service with its transaction queues.
    ///
    /// Provides both the gRPC queue (ArrayQueue) and P2P producer (rtrb)
    /// to match the production architecture.
    struct TestMempoolSetup {
        service: MempoolService,
        /// ArrayQueue for gRPC-submitted transactions (MPMC)
        tx_queue: Arc<ArrayQueue<Transaction>>,
        /// rtrb Producer for P2P-gossipped transactions (SPSC)
        #[allow(dead_code)]
        p2p_tx_producer: Producer<Transaction>,
        channels: MempoolChannels,
    }

    /// Creates a mempool service for testing with both transaction sources.
    ///
    /// The returned `TestMempoolSetup` includes:
    /// - `tx_queue`: ArrayQueue for gRPC transactions (used in most tests)
    /// - `p2p_tx_producer`: rtrb Producer for P2P transactions (available for P2P tests)
    fn spawn_test_mempool(
        reader: PendingStateReader,
        shutdown: Arc<AtomicBool>,
        logger: Logger,
    ) -> TestMempoolSetup {
        // Create gRPC transaction queue (ArrayQueue for MPMC)
        let tx_queue = Arc::new(ArrayQueue::new(TEST_BUFFER_SIZE));

        // Create P2P transaction channel (rtrb for SPSC)
        let (p2p_tx_producer, p2p_tx_consumer) = RingBuffer::<Transaction>::new(TEST_BUFFER_SIZE);

        // Spawn mempool with both sources
        let (service, channels) = MempoolService::spawn(
            Arc::clone(&tx_queue),
            p2p_tx_consumer,
            reader,
            shutdown,
            logger,
        );

        TestMempoolSetup {
            service,
            tx_queue,
            p2p_tx_producer,
            channels,
        }
    }

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

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        assert!(setup.service.is_running());

        setup.service.shutdown();

        assert!(setup.service.handle.is_none());
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

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        // Submit transactions with sequential nonces
        for nonce in 0..5 {
            let tx = create_tx(&sk, sender, recipient, 100, nonce, 10 + nonce);
            setup.tx_queue.push(tx).unwrap();
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
        setup.channels.proposal_req_producer.push(req).unwrap();

        // Wait for response
        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = setup.channels.proposal_resp_consumer.pop().unwrap();
        assert_eq!(resp.view, 1);
        assert_eq!(resp.transactions.len(), 5);

        setup.service.shutdown();
    }

    #[test]
    fn test_invalid_signature_rejected() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(&path).unwrap());
        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        let (sk, _sender) = gen_keypair();
        let (_, wrong_sender) = gen_keypair();
        let (_, recipient) = gen_keypair();

        // Create tx with mismatched sender/signature
        let invalid_tx = create_tx(&sk, wrong_sender, recipient, 100, 0, 10);
        setup.tx_queue.push(invalid_tx).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        setup.channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = setup.channels.proposal_resp_consumer.pop().unwrap();
        assert!(resp.transactions.is_empty());

        setup.service.shutdown();
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

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        // Submit transaction that exceeds balance (needs 210, has 100)
        let tx = create_tx(&sk, sender, recipient, 200, 0, 10);
        setup.tx_queue.push(tx).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        setup.channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = setup.channels.proposal_resp_consumer.pop().unwrap();
        assert!(resp.transactions.is_empty());

        setup.service.shutdown();
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

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        // Submit transactions with nonce gap (0, 2 - missing 1)
        // tx0 goes to pending, tx2 goes to queued
        let tx0 = create_tx(&sk, sender, recipient, 100, 0, 20);
        let tx2 = create_tx(&sk, sender, recipient, 100, 2, 10);

        setup.tx_queue.push(tx0).unwrap();
        setup.tx_queue.push(tx2).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        setup.channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = setup.channels.proposal_resp_consumer.pop().unwrap();
        // Only tx with nonce 0 should be included (tx2 is in queued pool)
        assert_eq!(resp.transactions.len(), 1);
        assert_eq!(resp.transactions[0].nonce, 0);

        setup.service.shutdown();
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

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        // Submit transactions out of order (2, 0, 1)
        // This tests that the pool promotes correctly
        let tx2 = create_tx(&sk, sender, recipient, 100, 2, 10);
        let tx0 = create_tx(&sk, sender, recipient, 100, 0, 30);
        let tx1 = create_tx(&sk, sender, recipient, 100, 1, 20);

        setup.tx_queue.push(tx2).unwrap(); // Goes to queued
        setup.tx_queue.push(tx0).unwrap(); // Goes to pending
        setup.tx_queue.push(tx1).unwrap(); // Goes to pending, promotes tx2

        std::thread::sleep(std::time::Duration::from_millis(100));

        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        setup.channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = setup.channels.proposal_resp_consumer.pop().unwrap();
        // All 3 should be included (tx2 was promoted when tx1 was added)
        assert_eq!(resp.transactions.len(), 3);

        setup.service.shutdown();
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

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        // sender1: fee 50, sender2: fee 100
        // sender2 should come first in proposal
        let tx1 = create_tx(&sk1, sender1, recipient, 100, 0, 50);
        let tx2 = create_tx(&sk2, sender2, recipient, 100, 0, 100);

        setup.tx_queue.push(tx1).unwrap();
        setup.tx_queue.push(tx2).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        setup.channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = setup.channels.proposal_resp_consumer.pop().unwrap();
        assert_eq!(resp.transactions.len(), 2);
        // Higher fee tx should come first
        assert_eq!(resp.transactions[0].fee, 100);
        assert_eq!(resp.transactions[1].fee, 50);

        setup.service.shutdown();
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

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        // Submit 10 transactions
        for nonce in 0..10 {
            let tx = create_tx(&sk, sender, recipient, 100, nonce, 10);
            setup.tx_queue.push(tx).unwrap();
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
        setup.channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = setup.channels.proposal_resp_consumer.pop().unwrap();
        // Should have fewer than 10 transactions due to size limit
        assert!(resp.transactions.len() < 10);
        assert!(!resp.transactions.is_empty());

        setup.service.shutdown();
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

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        // Submit 10 transactions
        for nonce in 0..10 {
            let tx = create_tx(&sk, sender, recipient, 100, nonce, 10);
            setup.tx_queue.push(tx).unwrap();
        }

        std::thread::sleep(std::time::Duration::from_millis(100));

        // Request with max_txs = 3
        let req = ProposalRequest {
            view: 1,
            max_txs: 3,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        setup.channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = setup.channels.proposal_resp_consumer.pop().unwrap();
        assert_eq!(resp.transactions.len(), 3);
        // Should be nonces 0, 1, 2
        assert_eq!(resp.transactions[0].nonce, 0);
        assert_eq!(resp.transactions[1].nonce, 1);
        assert_eq!(resp.transactions[2].nonce, 2);

        setup.service.shutdown();
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

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        // Submit 3 transactions, each costs 110 (100 amount + 10 fee)
        for nonce in 0..3 {
            let tx = create_tx(&sk, sender, recipient, 100, nonce, 10);
            setup.tx_queue.push(tx).unwrap();
        }

        std::thread::sleep(std::time::Duration::from_millis(100));

        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        setup.channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = setup.channels.proposal_resp_consumer.pop().unwrap();
        // Only 2 txs should be included (balance exhausted after 2)
        assert_eq!(resp.transactions.len(), 2);
        assert_eq!(resp.transactions[0].nonce, 0);
        assert_eq!(resp.transactions[1].nonce, 1);

        setup.service.shutdown();
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

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        // tx0: amount 500, fee 10 -> needs 510 (can't afford)
        // tx1: amount 50, fee 10 -> needs 60 (could afford if tx0 didn't exist)
        let tx0 = create_tx(&sk, sender, recipient, 500, 0, 10);
        let tx1 = create_tx(&sk, sender, recipient, 50, 1, 10);

        setup.tx_queue.push(tx0).unwrap();
        setup.tx_queue.push(tx1).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        setup.channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = setup.channels.proposal_resp_consumer.pop().unwrap();
        // Neither tx should be included:
        // - tx0 fails balance check
        // - tx1 is skipped because tx0 created a nonce gap
        assert!(resp.transactions.is_empty());

        setup.service.shutdown();
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

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        // Submit 5 transactions
        let mut tx_hashes = Vec::new();
        for nonce in 0..5 {
            let tx = create_tx(&sk, sender, recipient, 100, nonce, 10);
            tx_hashes.push(tx.tx_hash);
            setup.tx_queue.push(tx).unwrap();
        }

        std::thread::sleep(std::time::Duration::from_millis(100));

        // First proposal should have all 5
        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        setup.channels.proposal_req_producer.push(req).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = setup.channels.proposal_resp_consumer.pop().unwrap();
        assert_eq!(resp.transactions.len(), 5);

        // Finalize first 2 transactions
        let finalized = FinalizedNotification {
            view: 1,
            tx_hashes: vec![tx_hashes[0], tx_hashes[1]],
        };
        setup.channels.finalized_producer.push(finalized).unwrap();

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
        setup.channels.proposal_req_producer.push(req2).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp2 = setup.channels.proposal_resp_consumer.pop().unwrap();
        assert_eq!(resp2.transactions.len(), 3);
        // Should be nonces 2, 3, 4
        assert_eq!(resp2.transactions[0].nonce, 2);
        assert_eq!(resp2.transactions[1].nonce, 3);
        assert_eq!(resp2.transactions[2].nonce, 4);

        setup.service.shutdown();
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

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        // Submit transactions with nonces 0, 1, 2
        for nonce in 0..3 {
            let tx = create_tx(&sk, sender, recipient, 100, nonce, 10);
            setup.tx_queue.push(tx).unwrap();
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
        setup.channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = setup.channels.proposal_resp_consumer.pop().unwrap();
        // Only nonce 2 should be included (0 and 1 are stale)
        assert_eq!(resp.transactions.len(), 1);
        assert_eq!(resp.transactions[0].nonce, 2);

        setup.service.shutdown();
    }

    #[test]
    fn test_empty_proposal_on_empty_pool() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(&path).unwrap());
        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        // Request proposal without submitting any transactions
        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        setup.channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = setup.channels.proposal_resp_consumer.pop().unwrap();
        assert!(resp.transactions.is_empty());
        assert_eq!(resp.total_fees, 0);

        setup.service.shutdown();
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

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        // Submit transactions with fees 10, 20, 30
        for (nonce, fee) in [(0, 10), (1, 20), (2, 30)] {
            let tx = create_tx(&sk, sender, recipient, 100, nonce, fee);
            setup.tx_queue.push(tx).unwrap();
        }

        std::thread::sleep(std::time::Duration::from_millis(100));

        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        setup.channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = setup.channels.proposal_resp_consumer.pop().unwrap();
        assert_eq!(resp.transactions.len(), 3);
        assert_eq!(resp.total_fees, 60); // 10 + 20 + 30

        setup.service.shutdown();
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

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        // sender1 sends 500 to sender2 (fee 10)
        let tx1 = create_tx(&sk1, sender1, sender2, 500, 0, 10);
        // sender2 tries to send 400 (fee 10) - needs the 500 from tx1
        let tx2 = create_tx(&sk2, sender2, sender1, 400, 0, 10);

        setup.tx_queue.push(tx1).unwrap();
        setup.tx_queue.push(tx2).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        setup.channels.proposal_req_producer.push(req).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(100));

        let resp = setup.channels.proposal_resp_consumer.pop().unwrap();
        // Both transactions should be included:
        // - tx1: sender1 (1000) sends 500+10 to sender2
        // - tx2: sender2 (50 + 500 received = 550) sends 400+10
        assert_eq!(resp.transactions.len(), 2);

        setup.service.shutdown();
    }

    #[test]
    fn test_shutdown_instant_wakeup() {
        // Tests that shutdown wakes the thread immediately via unpark(),
        // rather than waiting for the 10ms idle timeout.
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(&path).unwrap());
        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        assert!(setup.service.is_running());

        // Let the service become idle and enter the park_timeout state
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Shutdown should complete quickly (< 15ms) because of instant unpark
        let start = std::time::Instant::now();
        setup.service.shutdown();
        let elapsed = start.elapsed();

        // If unpark didn't work, this would take ~10ms for the park_timeout
        // With unpark, it should be nearly instant (< 5ms typically)
        assert!(
            elapsed < std::time::Duration::from_millis(15),
            "Shutdown took {:?}, expected < 15ms (instant wakeup failed)",
            elapsed
        );

        assert!(setup.service.handle.is_none());
    }

    #[test]
    fn test_shutdown_handles_pending_proposal_requests() {
        // Tests that proposal requests in-flight during shutdown are still handled.
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(&path).unwrap());

        // Create and fund sender
        let (sk, sender) = gen_keypair();
        let (_, recipient) = gen_keypair();
        store
            .put_account(&Account::new(sk.public_key(), 10_000, 0))
            .unwrap();

        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let shutdown = Arc::new(AtomicBool::new(false));
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let mut setup = spawn_test_mempool(reader, Arc::clone(&shutdown), logger);

        // Submit a transaction
        let tx = create_tx(&sk, sender, recipient, 100, 0, 10);
        setup.tx_queue.push(tx).unwrap();

        // Wait for it to be processed into the pool
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Now signal shutdown, but first push a proposal request
        // The mempool should handle this request during graceful shutdown
        shutdown.store(true, Ordering::Release);

        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };
        setup.channels.proposal_req_producer.push(req).unwrap();

        // Wake up the thread to process shutdown
        setup.service.shutdown();

        // The response should have been produced during shutdown
        let resp = setup.channels.proposal_resp_consumer.pop();
        assert!(
            resp.is_ok(),
            "Expected proposal response to be handled during shutdown"
        );
        let resp = resp.unwrap();
        assert_eq!(resp.view, 1);
        assert_eq!(resp.transactions.len(), 1);
    }
}
