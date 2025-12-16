//! Mempool type definitions for inter-thread communication.
//!
//! These types are passed through lock-free rtrb ring buffers between
//! the consensus engine and the mempool service.

use std::{sync::Arc, time::Instant};

use crate::state::transaction::Transaction;

/// Request from consensus to mempool for block proposal transactions.
///
/// Sent by the consensus engine when it's the leader and needs to propose a block.
#[derive(Debug, Clone)]
pub struct ProposalRequest {
    /// View number for this proposal
    pub view: u64,
    /// Maximum number of transactions to include
    pub max_txs: usize,
    /// Maximum total size in bytes
    pub max_bytes: usize,
    /// Parent block hash (for context/logging)
    pub parent_block_hash: [u8; 32],
}

/// Response from mempool with transactions for block proposal.
///
/// Contains the selected transactions ordered by fee (highest first).
#[derive(Debug, Clone)]
pub struct ProposalResponse {
    /// View number (echoed from request)
    pub view: u64,
    /// Selected transactions, ordered by fee (highest first)
    pub transactions: Vec<Arc<Transaction>>,
    /// Total fees of included transactions
    pub total_fees: u64,
}

/// Notification from consensus that a block was finalized.
///
/// The mempool uses this to remove transactions that were included
/// in the finalized block, preventing them from being re-proposed.
#[derive(Debug, Clone)]
pub struct FinalizedNotification {
    /// View number of finalized block
    pub view: u64,
    /// Transaction hashes included in the finalized block
    pub tx_hashes: Vec<[u8; 32]>,
}

/// A transaction that has passed basic validation.
///
/// Stored in the mempool's priority queue with metadata for ordering.
#[derive(Debug, Clone)]
pub struct ValidatedTransaction {
    /// The validated transaction
    pub tx: Arc<Transaction>,
    /// When the transaction was received
    pub received_at: Instant,
    /// Priority score (typically the fee)
    pub priority: u64,
}

impl ValidatedTransaction {
    /// Creates a new validated transaction from a raw transaction.
    ///
    /// The priority is set to the transaction's fee.
    pub fn new(tx: Arc<Transaction>) -> Self {
        Self {
            priority: tx.fee,
            received_at: Instant::now(),
            tx,
        }
    }

    /// Creates a new validated transaction from a raw transaction with a received at time.
    pub fn new_with_received_at(tx: Arc<Transaction>, received_at: Instant) -> Self {
        Self {
            priority: tx.fee,
            received_at,
            tx,
        }
    }
}
