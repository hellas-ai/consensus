//! Transaction Pool - Fee-Priority Queue with Indexing
//!
//! Provides efficient storage and retrieval of transactions for block proposals.
//!
//! ## Features
//!
//! - **Fee-priority ordering**: Transactions sorted by fee (highest first) using `BTreeSet`
//! - **O(1) lookup**: By transaction hash via `HashMap`
//! - **Sender indexing**: Quick access to all transactions from a given sender
//! - **Automatic eviction**: Lowest-fee transactions evicted when at capacity
//! - **Zero-copy proposals**: Uses `Arc<Transaction>` for O(1) cloning
//!
//! ## Time Complexity
//!
//! | Operation        | Complexity |
//! |------------------|------------|
//! | `try_add`        | O(log n)   |
//! | `remove`         | O(log n)   |
//! | `build_proposal` | O(k)       |
//! | `min_fee`        | O(1)       |
//! | `evict_lowest`   | O(log n)   |
//!
//! Where n = pool size, k = transactions examined/selected.
//!
//! ## Memory Usage
//!
//! Approximately 150 bytes overhead per transaction plus the transaction itself.
//! For a pool of 10,000 transactions, expect ~1.5 MB of overhead.
//!
//! ## Thread Safety
//!
//! `TransactionPool` is **not** thread-safe. It is designed to be owned by a single
//! thread (the mempool service thread). Cross-thread communication uses lock-free
//! `rtrb` channels.

use std::{
    cmp::Ordering,
    collections::{BTreeSet, HashMap},
    sync::Arc,
    time::Instant,
};

use crate::state::address::Address;
use crate::state::transaction::Transaction;

use super::types::{
    FinalizedNotification, ProposalRequest, ProposalResponse, ValidatedTransaction,
};

/// Default maximum capacity of the transaction pool.
pub const DEFAULT_POOL_CAPACITY: usize = 10_000;

/// Transaction priority for heap ordering.
///
/// Higher fee = higher priority. Earlier arrival = higher priority (tie-breaker).
#[derive(Debug, Clone, Eq, PartialEq)]
struct TxPriority {
    /// Transaction fee (primary sort key)
    fee: u64,
    /// When the transaction was received (tie-breaker, earlier = higher priority)
    received_at: Instant,
    /// Transaction hash for lookup
    tx_hash: [u8; 32],
}

impl Ord for TxPriority {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher fee = comes first (reverse comparison)
        other
            .fee
            .cmp(&self.fee)
            // Earlier arrival = comes first (for same fee)
            .then_with(|| self.received_at.cmp(&other.received_at))
            // Tie-breaker by hash for total ordering
            .then_with(|| self.tx_hash.cmp(&other.tx_hash))
    }
}

impl PartialOrd for TxPriority {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Transaction pool with fee-based priority ordering.
pub struct TransactionPool {
    /// All transactions indexed by hash
    by_hash: HashMap<[u8; 32], ValidatedTransaction>,
    /// Priority queue (sorted by fee)
    by_priority: BTreeSet<TxPriority>,
    /// Transactions grouped by sender address
    by_sender: HashMap<Address, Vec<[u8; 32]>>,
    /// Maximum number of transactions to store
    capacity: usize,
    /// Statistics: total transactions added
    stats_added: u64,
    /// Statistics: total transactions removed (finalized or evicted)
    stats_removed: u64,
}

impl TransactionPool {
    /// Creates a new transaction pool with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            by_hash: HashMap::with_capacity(capacity),
            by_priority: BTreeSet::new(),
            by_sender: HashMap::new(),
            capacity,
            stats_added: 0,
            stats_removed: 0,
        }
    }

    /// Returns the current number of transactions in the pool.
    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    /// Returns true if the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }

    /// Returns the pool capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Attempts to add a transaction to the pool.
    ///
    /// The transaction must have already been validated (signature verified).
    ///
    /// # Returns
    ///
    /// - `true` if the transaction was added
    /// - `false` if the transaction was rejected (duplicate or lower fee than eviction threshold)
    pub fn try_add(&mut self, tx: Arc<Transaction>) -> bool {
        let tx_hash = tx.tx_hash;

        // Check for duplicates
        if self.by_hash.contains_key(&tx_hash) {
            return false;
        }

        // Check capacity and potentially evict
        if self.by_hash.len() >= self.capacity {
            // Only add if this transaction has higher fee than the lowest
            if let Some(min_fee) = self.min_fee() {
                if tx.fee <= min_fee {
                    return false; // Reject - not worth evicting
                }
            }
            self.evict_lowest();
        }

        // Create validated transaction
        let validated = ValidatedTransaction::new(tx.clone());
        let sender = validated.tx.sender;

        // Add to priority set (sorted by fee)
        self.by_priority.insert(TxPriority {
            fee: validated.priority,
            received_at: validated.received_at,
            tx_hash,
        });

        // Add to sender index
        self.by_sender.entry(sender).or_default().push(tx_hash);

        // Add to main storage
        self.by_hash.insert(tx_hash, validated);
        self.stats_added += 1;

        true
    }

    /// Builds a block proposal by selecting the highest-fee transactions.
    ///
    /// # Complexity
    /// O(k) where k is the number of transactions examined. No allocations
    /// except for the returned `selected` vector.
    pub fn build_proposal(&self, req: &ProposalRequest) -> ProposalResponse {
        let mut selected = Vec::with_capacity(req.max_txs);
        let mut total_bytes = 0usize;
        let mut total_fees = 0u64;

        // Iterate in priority order (highest fee first) - O(k), zero allocations
        for priority in &self.by_priority {
            if selected.len() >= req.max_txs {
                break;
            }

            // Get the transaction (should always exist since BTreeSet stays in sync)
            let Some(vtx) = self.by_hash.get(&priority.tx_hash) else {
                // Should never happen if we maintain invariants correctly
                continue;
            };

            // Check size constraint
            let tx_size = Self::estimate_tx_size(&vtx.tx);
            if total_bytes + tx_size > req.max_bytes {
                continue; // Skip this one, try next
            }

            // Accept this transaction
            selected.push(Arc::clone(&vtx.tx));
            total_bytes += tx_size;
            total_fees += vtx.tx.fee;
        }

        ProposalResponse {
            view: req.view,
            transactions: selected,
            total_fees,
        }
    }

    /// Returns an iterator over transactions in priority order (highest fee first).
    ///
    /// This is used by the service for state-validated proposal building,
    /// where nonces and balances are checked during iteration.
    pub fn iter_by_priority(&self) -> impl Iterator<Item = Arc<Transaction>> + '_ {
        self.by_priority
            .iter()
            .filter_map(|p| self.by_hash.get(&p.tx_hash).map(|vtx| Arc::clone(&vtx.tx)))
    }

    /// Removes transactions that were included in a finalized block.
    pub fn remove_finalized(&mut self, notif: &FinalizedNotification) {
        for tx_hash in &notif.tx_hashes {
            self.remove(tx_hash);
        }
    }

    /// Removes a transaction by its hash.
    ///
    /// Returns the removed transaction, if it existed.
    pub fn remove(&mut self, tx_hash: &[u8; 32]) -> Option<ValidatedTransaction> {
        if let Some(vtx) = self.by_hash.remove(tx_hash) {
            // Remove from priority set - O(log n)
            self.by_priority.remove(&TxPriority {
                fee: vtx.priority,
                received_at: vtx.received_at,
                tx_hash: *tx_hash,
            });

            // Remove from sender index
            if let Some(sender_txs) = self.by_sender.get_mut(&vtx.tx.sender) {
                sender_txs.retain(|h| h != tx_hash);
                if sender_txs.is_empty() {
                    self.by_sender.remove(&vtx.tx.sender);
                }
            }

            self.stats_removed += 1;
            Some(vtx)
        } else {
            None
        }
    }

    fn min_fee(&self) -> Option<u64> {
        // last() is now the lowest fee
        self.by_priority.last().map(|p| p.fee)
    }

    /// Evicts the lowest-fee transaction to make room for a new one.
    fn evict_lowest(&mut self) {
        // last() is the lowest fee
        if let Some(lowest) = self.by_priority.last().cloned() {
            self.remove(&lowest.tx_hash);
        }
    }

    /// Estimates the serialized size of a transaction.
    ///
    /// This is a rough estimate used for block size limiting.
    #[inline]
    fn estimate_tx_size(tx: &Transaction) -> usize {
        // Base size: sender (32) + instruction discriminant (1) + nonce (8) +
        // timestamp (8) + fee (8) + tx_hash (32) + signature (64) = 153 bytes
        // Plus instruction data (varies by type, ~40 bytes average)
        const BASE_SIZE: usize = 153;
        const INSTRUCTION_SIZE: usize = 40;

        // Suppress unused variable warning
        let _ = tx;

        BASE_SIZE + INSTRUCTION_SIZE
    }

    /// Returns the transactions for a specific sender.
    pub fn get_sender_transactions(&self, sender: &Address) -> Vec<&Transaction> {
        self.by_sender
            .get(sender)
            .map(|hashes| {
                hashes
                    .iter()
                    .filter_map(|h| self.by_hash.get(h).map(|vtx| &*vtx.tx))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Returns statistics about the pool.
    pub fn stats(&self) -> PoolStats {
        PoolStats {
            current_size: self.by_hash.len(),
            capacity: self.capacity,
            unique_senders: self.by_sender.len(),
            total_added: self.stats_added,
            total_removed: self.stats_removed,
        }
    }

    /// Clears all transactions from the pool.
    pub fn clear(&mut self) {
        self.by_hash.clear();
        self.by_priority.clear();
        self.by_sender.clear();
    }
}

/// Statistics about the transaction pool.
#[derive(Debug, Clone)]
pub struct PoolStats {
    /// Current number of transactions in the pool
    pub current_size: usize,
    /// Maximum capacity
    pub capacity: usize,
    /// Number of unique sender addresses
    pub unique_senders: usize,
    /// Total transactions added since creation
    pub total_added: u64,
    /// Total transactions removed since creation
    pub total_removed: u64,
}

impl Default for TransactionPool {
    fn default() -> Self {
        Self::new(DEFAULT_POOL_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::transaction_crypto::TxSecretKey;

    fn gen_keypair() -> (TxSecretKey, Address) {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let addr = Address::from_public_key(&pk);
        (sk, addr)
    }

    fn create_tx(
        sender_sk: &TxSecretKey,
        sender: Address,
        nonce: u64,
        fee: u64,
    ) -> Arc<Transaction> {
        let recipient = Address::from_bytes([nonce as u8; 32]);
        Arc::new(Transaction::new_transfer(
            sender, recipient, 100, nonce, fee, sender_sk,
        ))
    }

    #[test]
    fn test_add_and_retrieve() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();
        let tx = create_tx(&sk, sender, 0, 10);
        let tx_hash = tx.tx_hash;

        assert!(pool.try_add(tx));
        assert_eq!(pool.len(), 1);
        assert!(pool.by_hash.contains_key(&tx_hash));
    }

    #[test]
    fn test_duplicate_rejected() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();
        let tx = create_tx(&sk, sender, 0, 10);

        assert!(pool.try_add(tx.clone()));
        assert!(!pool.try_add(tx)); // Duplicate
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn test_capacity_eviction() {
        let mut pool = TransactionPool::new(3);
        let (sk, sender) = gen_keypair();

        // Add 3 transactions with different fees
        let tx1 = create_tx(&sk, sender, 0, 10);
        let tx2 = create_tx(&sk, sender, 1, 20);
        let tx3 = create_tx(&sk, sender, 2, 30);

        assert!(pool.try_add(tx1.clone()));
        assert!(pool.try_add(tx2));
        assert!(pool.try_add(tx3));
        assert_eq!(pool.len(), 3);

        // Add transaction with higher fee - should evict lowest (fee=10)
        let tx4 = create_tx(&sk, sender, 3, 40);
        assert!(pool.try_add(tx4));
        assert_eq!(pool.len(), 3);
        assert!(!pool.by_hash.contains_key(&tx1.tx_hash)); // Evicted

        // Add transaction with lower fee than minimum - should be rejected
        let tx5 = create_tx(&sk, sender, 4, 5);
        assert!(!pool.try_add(tx5));
        assert_eq!(pool.len(), 3);
    }

    #[test]
    fn test_build_proposal_priority_order() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add transactions in random fee order
        let tx_low = create_tx(&sk, sender, 0, 10);
        let tx_high = create_tx(&sk, sender, 1, 100);
        let tx_mid = create_tx(&sk, sender, 2, 50);

        pool.try_add(tx_low);
        pool.try_add(tx_high);
        pool.try_add(tx_mid);

        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };

        let resp = pool.build_proposal(&req);

        // Should be ordered by fee descending
        assert_eq!(resp.transactions.len(), 3);
        assert_eq!(resp.transactions[0].fee, 100);
        assert_eq!(resp.transactions[1].fee, 50);
        assert_eq!(resp.transactions[2].fee, 10);
        assert_eq!(resp.total_fees, 160);
    }

    #[test]
    fn test_build_proposal_respects_max_txs() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        for i in 0..10 {
            pool.try_add(create_tx(&sk, sender, i, 10 + i));
        }

        let req = ProposalRequest {
            view: 1,
            max_txs: 3,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };

        let resp = pool.build_proposal(&req);
        assert_eq!(resp.transactions.len(), 3);
        // Should have the 3 highest fee transactions
        assert_eq!(resp.transactions[0].fee, 19);
        assert_eq!(resp.transactions[1].fee, 18);
        assert_eq!(resp.transactions[2].fee, 17);
    }

    #[test]
    fn test_remove_finalized() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        let tx1 = create_tx(&sk, sender, 0, 10);
        let tx2 = create_tx(&sk, sender, 1, 20);
        let tx3 = create_tx(&sk, sender, 2, 30);

        pool.try_add(tx1.clone());
        pool.try_add(tx2.clone());
        pool.try_add(tx3);

        let notif = FinalizedNotification {
            view: 1,
            tx_hashes: vec![tx1.tx_hash, tx2.tx_hash],
        };

        pool.remove_finalized(&notif);

        assert_eq!(pool.len(), 1);
        assert!(!pool.by_hash.contains_key(&tx1.tx_hash));
        assert!(!pool.by_hash.contains_key(&tx2.tx_hash));
    }

    #[test]
    fn test_sender_index() {
        let mut pool = TransactionPool::new(100);

        let (sk1, sender1) = gen_keypair();
        let (sk2, sender2) = gen_keypair();

        pool.try_add(create_tx(&sk1, sender1, 0, 10));
        pool.try_add(create_tx(&sk1, sender1, 1, 20));
        pool.try_add(create_tx(&sk2, sender2, 0, 30));

        assert_eq!(pool.get_sender_transactions(&sender1).len(), 2);
        assert_eq!(pool.get_sender_transactions(&sender2).len(), 1);
    }

    #[test]
    fn test_empty_proposal() {
        let pool = TransactionPool::new(100);

        let req = ProposalRequest {
            view: 1,
            max_txs: 10,
            max_bytes: 100_000,
            parent_block_hash: [0u8; 32],
        };

        let resp = pool.build_proposal(&req);
        assert!(resp.transactions.is_empty());
        assert_eq!(resp.total_fees, 0);
        assert_eq!(resp.view, 1);
    }

    #[test]
    fn test_stats() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        let tx1 = create_tx(&sk, sender, 0, 10);
        let tx2 = create_tx(&sk, sender, 1, 20);

        pool.try_add(tx1.clone());
        pool.try_add(tx2);

        let stats = pool.stats();
        assert_eq!(stats.current_size, 2);
        assert_eq!(stats.capacity, 100);
        assert_eq!(stats.unique_senders, 1);
        assert_eq!(stats.total_added, 2);
        assert_eq!(stats.total_removed, 0);

        pool.remove(&tx1.tx_hash);

        let stats = pool.stats();
        assert_eq!(stats.current_size, 1);
        assert_eq!(stats.total_removed, 1);
    }
}
