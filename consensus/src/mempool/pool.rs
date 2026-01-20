//! Transaction Pool - Two-Pool Design (Ethereum-style)
//!
//! Transactions are separated into two pools:
//!
//! - **Pending**: Immediately executable transactions where nonce == expected
//! - **Queued**: Transactions waiting for prerequisites (nonce gap exists)
//!
//! ## Key Insight: Head-Based Fee Ordering
//!
//! Unlike a naive approach that puts ALL pending txs in a global fee-sorted set,
//! this implementation only tracks the HEAD (lowest nonce) tx per sender in the
//! fee-priority structure. This ensures block building always processes txs in
//! valid nonce order.
//!
//! ## Data Structures
//!
//! ┌─────────────────────────────────────────────────────────────┐
//! │                      TransactionPool                        │
//! ├─────────────────────────────────────────────────────────────┤
//! │  by_hash: HashMap<TxHash, ValidatedTransaction>             │
//! │    └─ O(1) lookup for any transaction                       │
//! │                                                             │
//! │  pending_by_sender: HashMap<Address, BTreeMap<nonce, hash>> │
//! │    └─ Per-sender pending txs, ordered by nonce              │
//! │                                                             │
//! │  sender_heads: BTreeSet<SenderHead>                         │
//! │    └─ ONE entry per sender: their HEAD (lowest nonce) tx    │
//! │    └─ Sorted by fee for block building                      │
//! │                                                             │
//! │  queued_by_sender: HashMap<Address, BTreeMap<nonce, hash>>  │
//! │    └─ Per-sender queued txs (not used for proposals)        │
//! └─────────────────────────────────────────────────────────────┘
//!
//! ## Block Building
//!
//! 1. Pop highest-fee sender head
//! 2. Include their head tx (guaranteed valid nonce)
//! 3. Advance to their next pending tx, reinsert in heap
//! 4. Repeat until block full
//!
//! ## Complexity
//!
//! | Operation        | Complexity    | Notes                                 |
//! |------------------|---------------|---------------------------------------|
//! | `try_add`        | O(log n)      | BTreeMap/BTreeSet insert              |
//! | `remove`         | O(log n)      | BTreeMap/BTreeSet remove              |
//! | `iter_pending`   | O(k log s)    | k = yielded txs, s = senders          |
//! | `promote_queued` | O(m log n)    | m = promoted txs                      |

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::Instant,
};

use crate::state::address::Address;
use crate::state::transaction::Transaction;

use super::types::{FinalizedNotification, ValidatedTransaction};

/// Default maximum capacity of the transaction pool.
pub const DEFAULT_POOL_CAPACITY: usize = 10_000;

/// Maximum queued transactions per sender.
const MAX_QUEUED_PER_SENDER: usize = 64;

/// Minimum fee bump percentage for replacing a queued transaction.
const MIN_FEE_BUMP_PERCENT: u64 = 110;

/// Represents a sender's "head" transaction in the fee-priority queue.
///
/// Only ONE entry exists per sender - their lowest-nonce pending tx.
/// Higher fee = higher priority. Earlier arrival = higher priority (tie-breaker).
#[derive(Debug, Clone, Eq, PartialEq)]
struct SenderHead {
    /// Transaction fee (primary sort key)
    fee: u64,
    /// When the transaction was received (tie-breaker, earlier = higher priority)
    received_at: Instant,
    /// The sender address (used to look up their pending txs)
    sender: Address,
}

impl Ord for SenderHead {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher fee = comes first (reverse comparison)
        other
            .fee
            .cmp(&self.fee)
            // Earlier arrival = comes first (for same fee)
            .then_with(|| self.received_at.cmp(&other.received_at))
            // Tie-breaker by sender bytes for total ordering
            .then_with(|| self.sender.as_bytes().cmp(other.sender.as_bytes()))
    }
}

impl PartialOrd for SenderHead {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Result of adding a transaction to the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddResult {
    /// Transaction added to pending pool (immediately executable)
    AddedPending,
    /// Transaction added to queued pool (waiting for nonce gap to fill)
    AddedQueued,
    /// Transaction rejected (duplicate, nonce too low, or capacity full)
    Rejected,
}

/// Transaction pool with two-pool design (pending + queued).
pub struct TransactionPool {
    /// All transactions indexed by hash (both pending and queued)
    by_hash: HashMap<[u8; 32], ValidatedTransaction>,

    /// Pending transactions per sender, ordered by nonce.
    /// Contains ALL consecutive pending nonces for each sender.
    pending_by_sender: HashMap<Address, BTreeMap<u64, [u8; 32]>>,

    /// Fee-priority ordering: ONE entry per sender (their HEAD tx).
    /// The "head" is the tx with the lowest nonce (first to execute).
    sender_heads: BTreeSet<SenderHead>,

    /// Queued transactions per sender, ordered by nonce
    queued_by_sender: HashMap<Address, BTreeMap<u64, [u8; 32]>>,

    /// Next expected nonce per sender (chain nonce + pending count).
    /// This is: base_nonce + number_of_consecutive_pending_txs
    pending_nonces: HashMap<Address, u64>,

    /// Maximum total transactions (pending + queued)
    capacity: usize,

    /// Total transactions added
    stats_added: u64,

    /// Total transactions removed
    stats_removed: u64,
}

impl TransactionPool {
    /// Creates a new transaction pool with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            by_hash: HashMap::with_capacity(capacity),
            pending_by_sender: HashMap::new(),
            sender_heads: BTreeSet::new(),
            queued_by_sender: HashMap::new(),
            pending_nonces: HashMap::new(),
            capacity,
            stats_added: 0,
            stats_removed: 0,
        }
    }

    /// Returns the base nonce (head tx nonce) for a sender, if they have pending txs.
    /// This is derived from `pending_by_sender` - O(1) via BTreeMap::first_key_value().
    #[inline]
    fn get_base_nonce(&self, sender: &Address) -> Option<u64> {
        self.pending_by_sender
            .get(sender)
            .and_then(|m| m.first_key_value())
            .map(|(&nonce, _)| nonce)
    }

    /// Returns the total number of transactions (pending + queued).
    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    /// Returns the number of pending (executable) transactions.
    pub fn pending_len(&self) -> usize {
        self.pending_by_sender.values().map(|m| m.len()).sum()
    }

    /// Returns the number of queued (waiting) transactions.
    pub fn queued_len(&self) -> usize {
        self.queued_by_sender.values().map(|m| m.len()).sum()
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
    /// The transaction is placed in either the pending or queued pool based on
    /// whether its nonce matches the expected nonce for that sender.
    ///
    /// # Arguments
    ///
    /// * `tx` - The transaction to add (must have valid signature)
    /// * `sender_base_nonce` - The sender's current nonce from chain state
    ///
    /// # Returns
    ///
    /// - `AddedPending` if added to pending pool
    /// - `AddedQueued` if added to queued pool
    /// - `Rejected` if duplicate, nonce too low, or pool full
    pub fn try_add(&mut self, tx: Arc<Transaction>, sender_base_nonce: u64) -> AddResult {
        let tx_hash = tx.tx_hash;
        let sender = tx.sender;
        let nonce = tx.nonce;

        // Check for duplicates
        if self.by_hash.contains_key(&tx_hash) {
            return AddResult::Rejected;
        }

        // Get or initialize expected nonce for this sender
        let expected_nonce = *self
            .pending_nonces
            .entry(sender)
            .or_insert(sender_base_nonce);

        // Reject if nonce is too low (already executed) OR handle pending replacement
        if nonce < expected_nonce {
            // Check if this nonce exists in pending - allow replacement if higher fee
            let replacement_result = self.try_replace_pending(&sender, nonce, &tx);
            match replacement_result {
                Some((true, new_received_at)) => {
                    // Successfully replaced - create validated tx and finish
                    let validated = ValidatedTransaction::new_with_received_at(tx, new_received_at);
                    self.by_hash.insert(tx_hash, validated);
                    self.stats_added += 1;
                    return AddResult::AddedPending;
                }
                Some((false, _)) => return AddResult::Rejected, // Fee too low
                None => return AddResult::Rejected,             /* Nonce not in pending (already
                                                                  * executed) */
            }
        }

        // Check if nonce already exists - allow replacement if higher fee
        if let Some(queued_txs) = self.queued_by_sender.get(&sender)
            && let Some(&existing_hash) = queued_txs.get(&nonce)
            && let Some(existing_vtx) = self.by_hash.get(&existing_hash)
        {
            let min_replacement_fee = existing_vtx.priority * MIN_FEE_BUMP_PERCENT / 100;
            if tx.fee >= min_replacement_fee {
                // Replace: remove old, continue to add new
                self.remove(&existing_hash);
            } else {
                return AddResult::Rejected; // Fee too low to replace
            }
        }

        // Check capacity
        if self.by_hash.len() >= self.capacity {
            // Try to evict lowest-fee pending tx
            if let Some(min_fee) = self.min_pending_fee()
                && tx.fee <= min_fee
            {
                return AddResult::Rejected;
            }
            self.evict_lowest_pending();
        }

        // Create validated transaction
        let validated = ValidatedTransaction::new(tx);

        // Add to main storage
        self.by_hash.insert(tx_hash, validated.clone());
        self.stats_added += 1;

        // Determine which pool to add to
        if nonce == expected_nonce {
            // Executable - add to pending
            self.add_to_pending(sender, nonce, tx_hash, &validated);

            // Update expected nonce
            self.pending_nonces.insert(sender, expected_nonce + 1);

            // Promote any queued transactions that are now executable
            self.promote_queued(&sender);

            AddResult::AddedPending
        } else {
            // Future nonce - add to queued
            self.add_to_queued(sender, nonce, tx_hash);

            // Limit queued per sender
            self.enforce_queued_limit(&sender);

            AddResult::AddedQueued
        }
    }

    /// Attempts to replace a pending transaction with the same nonce.
    /// Returns:
    /// - `Some(true)` if replacement succeeded (old tx removed, caller should add new)
    /// - `Some(false)` if replacement rejected (fee too low)
    /// - `None` if nonce not found in pending (already executed on chain)
    fn try_replace_pending(
        &mut self,
        sender: &Address,
        nonce: u64,
        new_tx: &Arc<Transaction>,
    ) -> Option<(bool, Instant)> {
        // First, gather info about the existing tx (immutable borrow)
        let (existing_hash, existing_fee, existing_received_at, is_head) = {
            let pending_txs = self.pending_by_sender.get(sender)?;
            let &existing_hash = pending_txs.get(&nonce)?;
            let existing_vtx = self.by_hash.get(&existing_hash)?;

            let is_head = pending_txs
                .first_key_value()
                .map(|(&first_nonce, _)| first_nonce == nonce)
                .unwrap_or(false);

            (
                existing_hash,
                existing_vtx.priority,
                existing_vtx.received_at,
                is_head,
            )
        };

        // Check fee requirement
        let min_replacement_fee = existing_fee * MIN_FEE_BUMP_PERCENT / 100;
        if new_tx.fee < min_replacement_fee {
            return Some((false, Instant::now())); // Fee too low
        }

        // Remove old tx from by_hash
        self.by_hash.remove(&existing_hash);
        self.stats_removed += 1;

        // Update hash in pending_by_sender (same nonce, new hash)
        if let Some(pending_txs) = self.pending_by_sender.get_mut(sender) {
            pending_txs.insert(nonce, new_tx.tx_hash);
        }

        let new_received_at = Instant::now();

        // If this was the head, update sender_heads
        if is_head {
            self.sender_heads.remove(&SenderHead {
                fee: existing_fee,
                received_at: existing_received_at,
                sender: *sender,
            });
            self.sender_heads.insert(SenderHead {
                fee: new_tx.fee,
                received_at: new_received_at,
                sender: *sender,
            });
        }

        Some((true, new_received_at))
    }

    /// Adds a transaction to the pending pool structures.
    ///
    /// Only updates `sender_heads` if this is the sender's first pending tx
    /// (i.e., they had no pending txs before, making this the new head).
    fn add_to_pending(
        &mut self,
        sender: Address,
        nonce: u64,
        tx_hash: [u8; 32],
        validated: &ValidatedTransaction,
    ) {
        // Check if sender already has pending txs (before we add)
        let had_pending = self.pending_by_sender.contains_key(&sender);

        // Add to pending_by_sender
        self.pending_by_sender
            .entry(sender)
            .or_default()
            .insert(nonce, tx_hash);

        // Only add to sender_heads if this is the first pending tx for this sender
        // (which means this tx IS the head)
        if !had_pending {
            self.sender_heads.insert(SenderHead {
                fee: validated.priority,
                received_at: validated.received_at,
                sender,
            });
            // NOTE: base nonce is derived from pending_by_sender.first_key_value()
        }
    }

    /// Adds a transaction to the queued pool.
    fn add_to_queued(&mut self, sender: Address, nonce: u64, tx_hash: [u8; 32]) {
        self.queued_by_sender
            .entry(sender)
            .or_default()
            .insert(nonce, tx_hash);
    }

    /// Promotes queued transactions to pending when they become executable.
    ///
    /// Called after adding a pending tx to check if any queued txs can now be promoted.
    /// NOTE: Does NOT update sender_heads since the head is already set.
    fn promote_queued(&mut self, sender: &Address) {
        loop {
            let expected_nonce = *self.pending_nonces.get(sender).unwrap_or(&0);

            // Check queued for next nonce
            let tx_hash = match self.queued_by_sender.get_mut(sender) {
                Some(queued_txs) => match queued_txs.remove(&expected_nonce) {
                    Some(hash) => hash,
                    None => break,
                },
                None => break,
            };

            // Clean up empty queued map
            if self
                .queued_by_sender
                .get(sender)
                .is_some_and(|q| q.is_empty())
            {
                self.queued_by_sender.remove(sender);
            }

            // Add to pending_by_sender (NOT to sender_heads - head is already set)
            self.pending_by_sender
                .entry(*sender)
                .or_default()
                .insert(expected_nonce, tx_hash);

            self.pending_nonces.insert(*sender, expected_nonce + 1);
        }
    }

    /// Enforces maximum queued transactions per sender.
    fn enforce_queued_limit(&mut self, sender: &Address) {
        let Some(queued_txs) = self.queued_by_sender.get_mut(sender) else {
            return;
        };

        while queued_txs.len() > MAX_QUEUED_PER_SENDER {
            // Remove highest nonce (least likely to be needed soon)
            if let Some((&highest_nonce, _)) = queued_txs.last_key_value()
                && let Some(&tx_hash) = queued_txs.get(&highest_nonce)
            {
                queued_txs.remove(&highest_nonce);
                self.by_hash.remove(&tx_hash);
                self.stats_removed += 1;
            }
        }
    }

    /// Returns an iterator over pending transactions for block building.
    ///
    /// Transactions are yielded in an order suitable for block inclusion:
    /// - Highest-fee sender's head tx first
    /// - Then their next nonce, or switch to next highest-fee sender
    /// - Nonces are always in order per sender
    ///
    /// # Algorithm
    ///
    /// Uses a heap of sender heads. Pop highest-fee sender, yield their head tx,
    /// then reinsert with their next pending tx's fee (if any).
    ///
    /// # Complexity
    ///
    /// - **Creation**: O(s) where s = senders with pending txs
    /// - **Iteration**: O(log s) per element
    pub fn iter_pending(&self) -> impl Iterator<Item = Arc<Transaction>> + '_ {
        PendingIter::new(self)
    }

    /// Removes transactions that were included in a finalized block.
    ///
    /// This also updates the base nonce for affected senders.
    pub fn remove_finalized(&mut self, notif: &FinalizedNotification) {
        for tx_hash in &notif.tx_hashes {
            self.remove(tx_hash);
        }
    }

    /// Updates the base nonce for a sender after chain state changes.
    ///
    /// Call this when blocks are finalized to update the expected nonces.
    /// This will remove any transactions with nonces below the new base.
    pub fn update_sender_nonce(&mut self, sender: &Address, new_base_nonce: u64) {
        let current_expected = *self.pending_nonces.get(sender).unwrap_or(&0);

        if new_base_nonce <= current_expected {
            return; // No change needed
        }

        // First, remove the old sender head (if exists)
        if let Some(old_base) = self.get_base_nonce(sender)
            && let Some(pending_txs) = self.pending_by_sender.get(sender)
            && let Some(&head_hash) = pending_txs.get(&old_base)
            && let Some(head_vtx) = self.by_hash.get(&head_hash)
        {
            self.sender_heads.remove(&SenderHead {
                fee: head_vtx.priority,
                received_at: head_vtx.received_at,
                sender: *sender,
            });
        }

        // Take ownership of the sender's pending txs (releases borrow on self)
        if let Some(mut pending_txs) = self.pending_by_sender.remove(sender) {
            // NOTE: `split_off` returns entries >= new_base_nonce, leaves < new_base_nonce in
            // original This is O(log n), not O(n)
            let to_keep = pending_txs.split_off(&new_base_nonce);

            // pending_txs now contains only entries with nonce < new_base_nonce (to remove)
            // NOTE: Iterate only the entries we're removing, which is O(k) where k = removed count
            for (_nonce, tx_hash) in pending_txs {
                if let Some(_vtx) = self.by_hash.remove(&tx_hash) {
                    self.stats_removed += 1;
                }
            }

            // Put back the entries to keep and update sender_heads
            if !to_keep.is_empty() {
                self.pending_by_sender.insert(*sender, to_keep.clone());

                // Update sender_heads with new head
                if let Some((&_new_head_nonce, &new_head_hash)) = to_keep.first_key_value()
                    && let Some(new_head_vtx) = self.by_hash.get(&new_head_hash)
                {
                    self.sender_heads.insert(SenderHead {
                        fee: new_head_vtx.priority,
                        received_at: new_head_vtx.received_at,
                        sender: *sender,
                    });
                    // NOTE: base nonce is derived from pending_by_sender
                }
            }
            // NOTE: no need to track base_nonces - derived from pending_by_sender
        }

        // Update expected nonce
        self.pending_nonces.insert(*sender, new_base_nonce);

        // Promote any queued that are now executable
        self.promote_queued(sender);
    }

    /// Removes a transaction by its hash from whichever pool it's in.
    pub fn remove(&mut self, tx_hash: &[u8; 32]) -> Option<ValidatedTransaction> {
        let vtx = self.by_hash.remove(tx_hash)?;
        let sender = vtx.tx.sender;
        let nonce = vtx.tx.nonce;

        // Try to remove from pending
        if let Some(pending_txs) = self.pending_by_sender.get_mut(&sender) {
            // Check if this is the head tx before we remove it
            let was_head = pending_txs
                .first_key_value()
                .map(|(&first_nonce, _)| first_nonce == nonce)
                .unwrap_or(false);

            if pending_txs.remove(&nonce).is_some() {
                if pending_txs.is_empty() {
                    // No more pending txs - remove from sender_heads entirely
                    self.pending_by_sender.remove(&sender);
                    self.sender_heads.remove(&SenderHead {
                        fee: vtx.priority,
                        received_at: vtx.received_at,
                        sender,
                    });
                } else if was_head {
                    // Update sender_heads with the new head tx
                    // First remove old head
                    self.sender_heads.remove(&SenderHead {
                        fee: vtx.priority,
                        received_at: vtx.received_at,
                        sender,
                    });

                    // Get new head (next lowest nonce)
                    if let Some((&_new_head_nonce, &new_head_hash)) = pending_txs.first_key_value()
                        && let Some(new_head_vtx) = self.by_hash.get(&new_head_hash)
                    {
                        self.sender_heads.insert(SenderHead {
                            fee: new_head_vtx.priority,
                            received_at: new_head_vtx.received_at,
                            sender,
                        });
                    }
                }

                self.stats_removed += 1;
                return Some(vtx);
            }
        }

        // Try to remove from queued
        if let Some(queued_txs) = self.queued_by_sender.get_mut(&sender)
            && queued_txs.remove(&nonce).is_some()
        {
            if queued_txs.is_empty() {
                self.queued_by_sender.remove(&sender);
            }

            self.stats_removed += 1;
            return Some(vtx);
        }

        self.stats_removed += 1;
        Some(vtx)
    }

    /// Returns the minimum fee among sender heads.
    ///
    /// Note: This returns the lowest HEAD fee, not the lowest overall fee.
    /// A sender might have a low-fee head but higher-fee subsequent txs.
    fn min_pending_fee(&self) -> Option<u64> {
        self.sender_heads.last().map(|h| h.fee)
    }

    /// Evicts the lowest-fee sender's head transaction.
    fn evict_lowest_pending(&mut self) {
        if let Some(lowest) = self.sender_heads.last().cloned() {
            let sender = lowest.sender;
            // Get the head tx hash for this sender (derived from pending_by_sender)
            if let Some(base_nonce) = self.get_base_nonce(&sender)
                && let Some(pending_txs) = self.pending_by_sender.get(&sender)
                && let Some(&tx_hash) = pending_txs.get(&base_nonce)
            {
                self.remove(&tx_hash);
            }
        }
    }

    /// Returns pending transactions for a specific sender, ordered by nonce.
    pub fn get_sender_pending(&self, sender: &Address) -> Vec<&Transaction> {
        self.pending_by_sender
            .get(sender)
            .map(|nonce_map| {
                nonce_map
                    .values()
                    .filter_map(|h| self.by_hash.get(h).map(|vtx| &*vtx.tx))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Returns an iterator over all senders with queued transactions.
    pub fn queued_senders(&self) -> impl Iterator<Item = Address> + '_ {
        self.queued_by_sender.keys().copied()
    }

    /// Returns queued transactions for a specific sender, ordered by nonce.
    pub fn get_sender_queued(&self, sender: &Address) -> Vec<&Transaction> {
        self.queued_by_sender
            .get(sender)
            .map(|nonce_map| {
                nonce_map
                    .values()
                    .filter_map(|h| self.by_hash.get(h).map(|vtx| &*vtx.tx))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Returns the expected nonce for a sender (base + pending count).
    pub fn get_pending_nonce(&self, sender: &Address) -> Option<u64> {
        self.pending_nonces.get(sender).copied()
    }

    /// Returns true if a transaction with the given hash exists in the pool.
    pub fn contains(&self, tx_hash: &[u8; 32]) -> bool {
        self.by_hash.contains_key(tx_hash)
    }

    /// Returns a reference to the validated transaction if it exists.
    pub fn get(&self, tx_hash: &[u8; 32]) -> Option<&ValidatedTransaction> {
        self.by_hash.get(tx_hash)
    }

    /// Returns statistics about the pool.
    pub fn stats(&self) -> PoolStats {
        let mut senders = self
            .pending_by_sender
            .keys()
            .collect::<std::collections::HashSet<_>>();
        senders.extend(self.queued_by_sender.keys());
        let unique_senders = senders.len();
        PoolStats {
            pending_size: self.pending_len(),
            queued_size: self.queued_len(),
            total_size: self.by_hash.len(),
            capacity: self.capacity,
            unique_senders,
            total_added: self.stats_added,
            total_removed: self.stats_removed,
        }
    }

    /// Clears all transactions from the pool.
    pub fn clear(&mut self) {
        self.by_hash.clear();
        self.pending_by_sender.clear();
        self.sender_heads.clear();
        self.queued_by_sender.clear();
        self.pending_nonces.clear();
    }
}

/// Iterator over pending transactions in block-building order.
///
/// Yields transactions by repeatedly:
/// 1. Popping the highest-fee sender from the heap
/// 2. Returning their current head tx
/// 3. Advancing to their next pending nonce and reinserting
struct PendingIter<'a> {
    pool: &'a TransactionPool,
    /// Working heap of senders, ordered by their current head's fee
    heap: BTreeSet<SenderHead>,
    /// Current nonce being processed per sender
    current_nonces: HashMap<Address, u64>,
}

impl<'a> PendingIter<'a> {
    fn new(pool: &'a TransactionPool) -> Self {
        // Clone the sender_heads as our working heap
        let heap = pool.sender_heads.clone();
        // Derive current nonces from pending_by_sender (first key = base nonce)
        let current_nonces: HashMap<Address, u64> = pool
            .pending_by_sender
            .iter()
            .filter_map(|(addr, nonces)| nonces.first_key_value().map(|(&nonce, _)| (*addr, nonce)))
            .collect();
        Self {
            pool,
            heap,
            current_nonces,
        }
    }
}

impl<'a> Iterator for PendingIter<'a> {
    type Item = Arc<Transaction>;

    fn next(&mut self) -> Option<Self::Item> {
        // Pop the highest-fee sender (first in BTreeSet due to Ord impl)
        let head = self.heap.pop_first()?;
        let sender = head.sender;

        // Get current nonce for this sender
        let current_nonce = self.current_nonces.get(&sender).copied()?;

        // Get the tx at this nonce
        let pending_txs = self.pool.pending_by_sender.get(&sender)?;
        let tx_hash = pending_txs.get(&current_nonce)?;
        let vtx = self.pool.by_hash.get(tx_hash)?;
        let tx = Arc::clone(&vtx.tx);

        // Advance to next nonce
        let next_nonce = current_nonce + 1;
        self.current_nonces.insert(sender, next_nonce);

        // If sender has more pending txs, reinsert with next tx's fee
        if let Some(next_hash) = pending_txs.get(&next_nonce)
            && let Some(next_vtx) = self.pool.by_hash.get(next_hash)
        {
            self.heap.insert(SenderHead {
                fee: next_vtx.priority,
                received_at: next_vtx.received_at,
                sender,
            });
        }

        Some(tx)
    }
}

/// Statistics about the transaction pool.
#[derive(Debug, Clone)]
pub struct PoolStats {
    /// Number of pending (executable) transactions
    pub pending_size: usize,
    /// Number of queued (waiting) transactions
    pub queued_size: usize,
    /// Total transactions (pending + queued)
    pub total_size: usize,
    /// Maximum capacity
    pub capacity: usize,
    /// Number of unique sender addresses
    pub unique_senders: usize,
    /// Total transactions added since creation
    pub total_added: u64,
    /// Total transactions removed since creation
    pub total_removed: u64,
}

// Backwards compatibility alias
impl PoolStats {
    pub fn current_size(&self) -> usize {
        self.total_size
    }
}

impl Default for PoolStats {
    fn default() -> Self {
        Self {
            pending_size: 0,
            queued_size: 0,
            total_size: 0,
            capacity: 10_000,
            unique_senders: 0,
            total_added: 0,
            total_removed: 0,
        }
    }
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
    fn test_add_to_pending() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add tx with nonce 0, base nonce is 0 -> should go to pending
        let tx = create_tx(&sk, sender, 0, 10);
        assert_eq!(pool.try_add(tx.clone(), 0), AddResult::AddedPending);
        assert_eq!(pool.pending_len(), 1);
        assert_eq!(pool.queued_len(), 0);
        assert_eq!(pool.get_pending_nonce(&sender), Some(1));
    }

    #[test]
    fn test_add_to_queued() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add tx with nonce 5, base nonce is 0 -> should go to queued
        let tx = create_tx(&sk, sender, 5, 10);
        assert_eq!(pool.try_add(tx, 0), AddResult::AddedQueued);
        assert_eq!(pool.pending_len(), 0);
        assert_eq!(pool.queued_len(), 1);
    }

    #[test]
    fn test_reject_low_nonce() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add tx with nonce 0, but base nonce is 5 -> should reject
        let tx = create_tx(&sk, sender, 0, 10);
        assert_eq!(pool.try_add(tx, 5), AddResult::Rejected);
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn test_promotion_on_add() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add nonces 1, 2, 3 first (all go to queued)
        for nonce in 1..=3 {
            let tx = create_tx(&sk, sender, nonce, 10 + nonce);
            assert_eq!(pool.try_add(tx, 0), AddResult::AddedQueued);
        }
        assert_eq!(pool.pending_len(), 0);
        assert_eq!(pool.queued_len(), 3);

        // Add nonce 0 -> should trigger promotion of 1, 2, 3
        let tx0 = create_tx(&sk, sender, 0, 100);
        assert_eq!(pool.try_add(tx0, 0), AddResult::AddedPending);

        // All 4 should now be pending
        assert_eq!(pool.pending_len(), 4);
        assert_eq!(pool.queued_len(), 0);
        assert_eq!(pool.get_pending_nonce(&sender), Some(4));
    }

    #[test]
    fn test_iter_pending_nonce_order_per_sender() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add transactions with sequential nonces and varying fees
        let tx0 = create_tx(&sk, sender, 0, 10);
        let tx1 = create_tx(&sk, sender, 1, 100);
        let tx2 = create_tx(&sk, sender, 2, 50);

        pool.try_add(tx0, 0);
        pool.try_add(tx1, 0);
        pool.try_add(tx2, 0);

        // For a SINGLE sender, iter_pending returns in NONCE order (not fee order!)
        // This is correct because you can't execute nonce 1 before nonce 0.
        let nonces: Vec<u64> = pool.iter_pending().map(|tx| tx.nonce).collect();
        assert_eq!(nonces, vec![0, 1, 2]);
    }

    #[test]
    fn test_sequential_nonces_any_fee_order() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add 5 transactions with sequential nonces, increasing fees
        // This is the case that failed before!
        for nonce in 0..5u64 {
            let tx = create_tx(&sk, sender, nonce, 10 + nonce);
            pool.try_add(tx, 0);
        }

        // All 5 should be pending
        assert_eq!(pool.pending_len(), 5);
        assert_eq!(pool.queued_len(), 0);

        // Collect all pending txs
        let pending: Vec<_> = pool.iter_pending().collect();
        assert_eq!(pending.len(), 5);
    }

    #[test]
    fn test_duplicate_rejected() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();
        let tx = create_tx(&sk, sender, 0, 10);

        assert_eq!(pool.try_add(tx.clone(), 0), AddResult::AddedPending);
        assert_eq!(pool.try_add(tx, 0), AddResult::Rejected);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn test_pending_replace_by_fee_head() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add tx to pending as head (nonce 0, fee 100)
        let tx1 = create_tx(&sk, sender, 0, 100);
        assert_eq!(pool.try_add(tx1.clone(), 0), AddResult::AddedPending);
        assert_eq!(pool.pending_len(), 1);

        // Try replace with same fee - should fail
        let tx2 = create_tx(&sk, sender, 0, 100);
        assert_eq!(pool.try_add(tx2, 0), AddResult::Rejected);

        // Try replace with 109% fee - should fail (need 110%)
        let tx3 = create_tx(&sk, sender, 0, 109);
        assert_eq!(pool.try_add(tx3, 0), AddResult::Rejected);

        // Replace with 110% fee - should succeed
        let tx4 = create_tx(&sk, sender, 0, 110);
        assert_eq!(pool.try_add(tx4.clone(), 0), AddResult::AddedPending);
        assert_eq!(pool.pending_len(), 1);

        // Old tx should be gone, new one present
        assert!(!pool.contains(&tx1.tx_hash));
        assert!(pool.contains(&tx4.tx_hash));

        // Verify the new tx is returned by iter_pending
        let fees: Vec<u64> = pool.iter_pending().map(|tx| tx.fee).collect();
        assert_eq!(fees, vec![110]);
    }

    #[test]
    fn test_pending_replace_by_fee_non_head() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add nonces 0, 1, 2 to pending
        let tx0 = create_tx(&sk, sender, 0, 50);
        let tx1 = create_tx(&sk, sender, 1, 100);
        let tx2 = create_tx(&sk, sender, 2, 75);
        pool.try_add(tx0.clone(), 0);
        pool.try_add(tx1.clone(), 0);
        pool.try_add(tx2.clone(), 0);
        assert_eq!(pool.pending_len(), 3);

        // Replace nonce 1 (not head) with higher fee
        let tx1_new = create_tx(&sk, sender, 1, 150);
        assert_eq!(pool.try_add(tx1_new.clone(), 0), AddResult::AddedPending);
        assert_eq!(pool.pending_len(), 3);

        // Old tx should be gone, new one present
        assert!(!pool.contains(&tx1.tx_hash));
        assert!(pool.contains(&tx1_new.tx_hash));

        // Head should still be tx0 (nonce 0), so first returned should have fee 50
        let first_tx = pool.iter_pending().next().unwrap();
        assert_eq!(first_tx.fee, 50);
        assert_eq!(first_tx.nonce, 0);
    }

    #[test]
    fn test_pending_replace_updates_sender_heads() {
        let mut pool = TransactionPool::new(100);
        let (sk1, sender1) = gen_keypair();
        let (sk2, sender2) = gen_keypair();

        // sender1: nonce 0, fee 50 (head)
        // sender2: nonce 0, fee 100 (head)
        pool.try_add(create_tx(&sk1, sender1, 0, 50), 0);
        pool.try_add(create_tx(&sk2, sender2, 0, 100), 0);

        // Initially sender2's tx comes first (higher fee)
        let fees: Vec<u64> = pool.iter_pending().map(|tx| tx.fee).collect();
        assert_eq!(fees, vec![100, 50]);

        // Replace sender1's head with fee 200 (200 >= 50 * 110%)
        let tx1_new = create_tx(&sk1, sender1, 0, 200);
        assert_eq!(pool.try_add(tx1_new, 0), AddResult::AddedPending);

        // Now sender1's tx should come first (higher fee after replacement)
        let fees: Vec<u64> = pool.iter_pending().map(|tx| tx.fee).collect();
        assert_eq!(fees, vec![200, 100]);
    }

    #[test]
    fn test_pending_replace_already_executed_nonce() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add nonce 5 with base nonce 5 (so nonces 0-4 are "executed")
        pool.try_add(create_tx(&sk, sender, 5, 100), 5);
        assert_eq!(pool.pending_len(), 1);

        // Try to replace nonce 2 - should fail (already executed on chain)
        let tx_old_nonce = create_tx(&sk, sender, 2, 200);
        assert_eq!(pool.try_add(tx_old_nonce, 5), AddResult::Rejected);
    }

    #[test]
    fn test_pending_replace_preserves_nonce_order() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add nonces 0, 1, 2, 3, 4 to pending
        for nonce in 0..5 {
            pool.try_add(create_tx(&sk, sender, nonce, 10 + nonce), 0);
        }
        assert_eq!(pool.pending_len(), 5);

        // Replace nonce 2 with higher fee
        let tx2_new = create_tx(&sk, sender, 2, 500);
        assert_eq!(pool.try_add(tx2_new, 0), AddResult::AddedPending);

        // All txs should still be pending in nonce order
        let nonces: Vec<u64> = pool.iter_pending().map(|tx| tx.nonce).collect();
        assert_eq!(nonces, vec![0, 1, 2, 3, 4]);

        // Verify the replaced tx has the new fee
        let fees: Vec<u64> = pool.iter_pending().map(|tx| tx.fee).collect();
        assert_eq!(fees[2], 500); // nonce 2 should have fee 500
    }

    #[test]
    fn test_remove_finalized() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        let tx0 = create_tx(&sk, sender, 0, 10);
        let tx1 = create_tx(&sk, sender, 1, 20);
        let tx2 = create_tx(&sk, sender, 2, 30);

        pool.try_add(tx0.clone(), 0);
        pool.try_add(tx1.clone(), 0);
        pool.try_add(tx2, 0);

        let notif = FinalizedNotification {
            view: 1,
            tx_hashes: vec![tx0.tx_hash, tx1.tx_hash],
        };

        pool.remove_finalized(&notif);
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.pending_len(), 1);
    }

    #[test]
    fn test_update_sender_nonce() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add nonces 0, 1, 2 as pending
        for nonce in 0..3 {
            pool.try_add(create_tx(&sk, sender, nonce, 10), 0);
        }

        // Add nonces 5, 6 as queued
        pool.try_add(create_tx(&sk, sender, 5, 10), 0);
        pool.try_add(create_tx(&sk, sender, 6, 10), 0);

        assert_eq!(pool.pending_len(), 3);
        assert_eq!(pool.queued_len(), 2);

        // Update sender nonce to 5 (simulates nonces 0-4 being mined elsewhere)
        pool.update_sender_nonce(&sender, 5);

        // Nonces 0, 1, 2 should be removed, 5, 6 promoted
        assert_eq!(pool.pending_len(), 2);
        assert_eq!(pool.queued_len(), 0);
        assert_eq!(pool.get_pending_nonce(&sender), Some(7));
    }

    #[test]
    fn test_capacity_eviction() {
        let mut pool = TransactionPool::new(3);
        let (sk, sender) = gen_keypair();

        pool.try_add(create_tx(&sk, sender, 0, 10), 0);
        pool.try_add(create_tx(&sk, sender, 1, 20), 0);
        pool.try_add(create_tx(&sk, sender, 2, 30), 0);
        assert_eq!(pool.len(), 3);

        // Add higher fee tx - should evict fee=10
        pool.try_add(create_tx(&sk, sender, 3, 40), 0);
        assert_eq!(pool.len(), 3);

        // Verify lowest fee was evicted
        let fees: Vec<u64> = pool.iter_pending().map(|tx| tx.fee).collect();
        assert!(!fees.contains(&10));
        assert!(fees.contains(&40));
    }

    #[test]
    fn test_stats() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        pool.try_add(create_tx(&sk, sender, 0, 10), 0);
        pool.try_add(create_tx(&sk, sender, 1, 20), 0);
        pool.try_add(create_tx(&sk, sender, 5, 30), 0); // Queued

        let stats = pool.stats();
        assert_eq!(stats.pending_size, 2);
        assert_eq!(stats.queued_size, 1);
        assert_eq!(stats.total_size, 3);
        assert_eq!(stats.total_added, 3);
    }

    #[test]
    fn test_multi_sender_independent_nonces() {
        let mut pool = TransactionPool::new(100);
        let (sk1, sender1) = gen_keypair();
        let (sk2, sender2) = gen_keypair();

        // Both senders start at nonce 0
        pool.try_add(create_tx(&sk1, sender1, 0, 10), 0);
        pool.try_add(create_tx(&sk2, sender2, 0, 20), 0);

        assert_eq!(pool.pending_len(), 2);
        assert_eq!(pool.get_pending_nonce(&sender1), Some(1));
        assert_eq!(pool.get_pending_nonce(&sender2), Some(1));
    }

    #[test]
    fn test_multi_sender_fee_ordering() {
        let mut pool = TransactionPool::new(100);
        let (sk1, sender1) = gen_keypair();
        let (sk2, sender2) = gen_keypair();

        // sender1: nonce 0, fee 50
        // sender2: nonce 0, fee 100
        pool.try_add(create_tx(&sk1, sender1, 0, 50), 0);
        pool.try_add(create_tx(&sk2, sender2, 0, 100), 0);

        // iter_pending should return sender2's tx first (higher fee)
        let fees: Vec<u64> = pool.iter_pending().map(|tx| tx.fee).collect();
        assert_eq!(fees, vec![100, 50]);
    }

    #[test]
    fn test_multi_sender_mixed_pending_queued() {
        let mut pool = TransactionPool::new(100);
        let (sk1, sender1) = gen_keypair();
        let (sk2, sender2) = gen_keypair();

        // sender1: pending nonce 0
        pool.try_add(create_tx(&sk1, sender1, 0, 10), 0);
        // sender2: queued nonce 5
        pool.try_add(create_tx(&sk2, sender2, 5, 10), 0);

        assert_eq!(pool.pending_len(), 1);
        assert_eq!(pool.queued_len(), 1);

        let stats = pool.stats();
        assert_eq!(stats.unique_senders, 2);
    }

    #[test]
    fn test_update_sender_nonce_noop_same() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        pool.try_add(create_tx(&sk, sender, 0, 10), 0);
        pool.try_add(create_tx(&sk, sender, 1, 20), 0);
        assert_eq!(pool.pending_len(), 2);

        // Update to current nonce - should be no-op
        pool.update_sender_nonce(&sender, 0);
        assert_eq!(pool.pending_len(), 2);

        // Update to same as pending nonce - should be no-op
        pool.update_sender_nonce(&sender, 2);
        assert_eq!(pool.pending_len(), 2);
    }

    #[test]
    fn test_update_sender_nonce_noop_lower() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        pool.try_add(create_tx(&sk, sender, 5, 10), 5);
        assert_eq!(pool.pending_len(), 1);
        assert_eq!(pool.get_pending_nonce(&sender), Some(6));

        // Update to LOWER nonce - should be no-op
        pool.update_sender_nonce(&sender, 3);
        assert_eq!(pool.pending_len(), 1);
        assert_eq!(pool.get_pending_nonce(&sender), Some(6));
    }

    #[test]
    fn test_update_sender_nonce_removes_all_pending() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add nonces 0, 1, 2
        for nonce in 0..3 {
            pool.try_add(create_tx(&sk, sender, nonce, 10), 0);
        }
        assert_eq!(pool.pending_len(), 3);

        // Update to nonce 10 - removes all pending
        pool.update_sender_nonce(&sender, 10);

        assert_eq!(pool.pending_len(), 0);
        assert_eq!(pool.get_pending_nonce(&sender), Some(10));

        // Sender should be cleaned up from pending_by_sender
        assert!(!pool.pending_by_sender.contains_key(&sender));
    }

    #[test]
    fn test_update_sender_nonce_partial_promotion() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add nonces 0, 1 as pending
        pool.try_add(create_tx(&sk, sender, 0, 10), 0);
        pool.try_add(create_tx(&sk, sender, 1, 10), 0);

        // Add nonces 5, 6, 10 as queued (gap at 7,8,9)
        pool.try_add(create_tx(&sk, sender, 5, 10), 0);
        pool.try_add(create_tx(&sk, sender, 6, 10), 0);
        pool.try_add(create_tx(&sk, sender, 10, 10), 0);

        assert_eq!(pool.pending_len(), 2);
        assert_eq!(pool.queued_len(), 3);

        // Update to nonce 5 - removes 0,1 from pending, promotes 5,6 from queued
        pool.update_sender_nonce(&sender, 5);

        assert_eq!(pool.pending_len(), 2); // 5 and 6
        assert_eq!(pool.queued_len(), 1); // 10 still queued
        assert_eq!(pool.get_pending_nonce(&sender), Some(7));
    }

    #[test]
    fn test_update_sender_nonce_unknown_sender() {
        let mut pool = TransactionPool::new(100);
        let (_, sender) = gen_keypair();

        // Should not panic on unknown sender
        pool.update_sender_nonce(&sender, 5);
        assert_eq!(pool.get_pending_nonce(&sender), Some(5));
    }

    #[test]
    fn test_queued_limit_enforcement() {
        let mut pool = TransactionPool::new(1000);
        let (sk, sender) = gen_keypair();

        // Add MAX_QUEUED_PER_SENDER + 10 transactions to queued
        for nonce in 1..=(MAX_QUEUED_PER_SENDER + 10) as u64 {
            pool.try_add(create_tx(&sk, sender, nonce, 10), 0);
        }

        // Should be capped at MAX_QUEUED_PER_SENDER
        assert_eq!(pool.queued_len(), MAX_QUEUED_PER_SENDER);
    }

    #[test]
    fn test_empty_pool_operations() {
        let pool = TransactionPool::new(100);

        assert_eq!(pool.len(), 0);
        assert_eq!(pool.pending_len(), 0);
        assert_eq!(pool.queued_len(), 0);
        assert!(pool.iter_pending().next().is_none());

        let stats = pool.stats();
        assert_eq!(stats.total_size, 0);
        assert_eq!(stats.unique_senders, 0);
    }

    #[test]
    fn test_capacity_one() {
        let mut pool = TransactionPool::new(1);
        let (sk, sender) = gen_keypair();

        let tx0 = create_tx(&sk, sender, 0, 10);
        let tx1 = create_tx(&sk, sender, 1, 100);

        pool.try_add(tx0, 0);
        assert_eq!(pool.len(), 1);

        // Adding higher fee should evict lower
        pool.try_add(tx1, 0);
        assert_eq!(pool.len(), 1);

        let fees: Vec<u64> = pool.iter_pending().map(|tx| tx.fee).collect();
        assert_eq!(fees, vec![100]);
    }

    #[test]
    fn test_remove_nonexistent() {
        let mut pool = TransactionPool::new(100);
        let fake_hash = [42u8; 32];

        // Should not panic
        let removed = pool.remove(&fake_hash);
        assert!(removed.is_none());
    }

    #[test]
    fn test_contains_and_get() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();
        let tx = create_tx(&sk, sender, 0, 10);
        let hash = tx.tx_hash;

        assert!(!pool.contains(&hash));
        assert!(pool.get(&hash).is_none());

        pool.try_add(tx.clone(), 0);

        assert!(pool.contains(&hash));
        assert!(pool.get(&hash).is_some());
        assert_eq!(pool.get(&hash).unwrap().tx.tx_hash, hash);
    }

    #[test]
    fn test_clear() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        for nonce in 0..10 {
            pool.try_add(create_tx(&sk, sender, nonce, 10), 0);
        }
        assert_eq!(pool.len(), 10);

        pool.clear();

        assert_eq!(pool.len(), 0);
        assert_eq!(pool.pending_len(), 0);
        assert_eq!(pool.queued_len(), 0);
        assert!(pool.get_pending_nonce(&sender).is_none());
    }

    #[test]
    fn test_promotion_with_gaps_in_queued() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add nonces 2, 4, 6 to queued (all have gaps)
        pool.try_add(create_tx(&sk, sender, 2, 10), 0);
        pool.try_add(create_tx(&sk, sender, 4, 20), 0);
        pool.try_add(create_tx(&sk, sender, 6, 30), 0);
        assert_eq!(pool.queued_len(), 3);

        // Add nonce 0 - should promote only 0, NOT 2,4,6 (gap at 1)
        pool.try_add(create_tx(&sk, sender, 0, 5), 0);
        assert_eq!(pool.pending_len(), 1);
        assert_eq!(pool.queued_len(), 3);

        // Add nonce 1 - should promote 1, 2, then stop (gap at 3)
        pool.try_add(create_tx(&sk, sender, 1, 5), 0);
        assert_eq!(pool.pending_len(), 3); // 0, 1, 2
        assert_eq!(pool.queued_len(), 2); // 4, 6
    }

    #[test]
    fn test_large_promotion_chain() {
        let mut pool = TransactionPool::new(1000);
        let (sk, sender) = gen_keypair();

        // Add MAX_QUEUED_PER_SENDER transactions to queued
        for nonce in 1..=(MAX_QUEUED_PER_SENDER as u64) {
            pool.try_add(create_tx(&sk, sender, nonce, 10), 0);
        }
        assert_eq!(pool.queued_len(), MAX_QUEUED_PER_SENDER);
        assert_eq!(pool.pending_len(), 0);

        // Add nonce 0 - should promote all
        pool.try_add(create_tx(&sk, sender, 0, 10), 0);
        assert_eq!(pool.pending_len(), MAX_QUEUED_PER_SENDER + 1);
        assert_eq!(pool.queued_len(), 0);
        assert_eq!(
            pool.get_pending_nonce(&sender),
            Some((MAX_QUEUED_PER_SENDER + 1) as u64)
        );
    }

    #[test]
    fn test_remove_from_pending() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        let tx = create_tx(&sk, sender, 0, 10);
        let hash = tx.tx_hash;
        pool.try_add(tx, 0);

        assert_eq!(pool.pending_len(), 1);

        let removed = pool.remove(&hash);
        assert!(removed.is_some());
        assert_eq!(pool.pending_len(), 0);
        assert!(!pool.contains(&hash));
    }

    #[test]
    fn test_remove_from_queued() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        let tx = create_tx(&sk, sender, 5, 10); // Will be queued (gap)
        let hash = tx.tx_hash;
        pool.try_add(tx, 0);

        assert_eq!(pool.queued_len(), 1);

        let removed = pool.remove(&hash);
        assert!(removed.is_some());
        assert_eq!(pool.queued_len(), 0);
        assert!(!pool.contains(&hash));
    }

    #[test]
    fn test_pending_replace_then_remove() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add head tx
        let tx1 = create_tx(&sk, sender, 0, 100);
        pool.try_add(tx1.clone(), 0);
        assert_eq!(pool.sender_heads.len(), 1);

        // Replace head with higher fee
        let tx2 = create_tx(&sk, sender, 0, 200);
        pool.try_add(tx2.clone(), 0);
        assert_eq!(pool.sender_heads.len(), 1); // Still 1 entry

        // Remove the replaced tx
        pool.remove(&tx2.tx_hash);

        // sender_heads should be empty (no stale entries)
        assert_eq!(pool.sender_heads.len(), 0);
        assert_eq!(pool.pending_len(), 0);
    }

    #[test]
    fn test_eviction_across_senders() {
        let mut pool = TransactionPool::new(3);
        let (sk1, sender1) = gen_keypair();
        let (sk2, sender2) = gen_keypair();

        // sender1: nonce 0, fee 10
        // sender2: nonce 0, fee 20
        // sender1: nonce 1, fee 30
        pool.try_add(create_tx(&sk1, sender1, 0, 10), 0);
        pool.try_add(create_tx(&sk2, sender2, 0, 20), 0);
        pool.try_add(create_tx(&sk1, sender1, 1, 30), 0);
        assert_eq!(pool.len(), 3);

        // Add sender2: nonce 1, fee 100 - should evict lowest (fee=10)
        pool.try_add(create_tx(&sk2, sender2, 1, 100), 0);
        assert_eq!(pool.len(), 3);

        let fees: Vec<u64> = pool.iter_pending().map(|tx| tx.fee).collect();
        assert!(!fees.contains(&10));
        assert!(fees.contains(&100));
    }

    #[test]
    fn test_queued_replace_by_fee() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add tx to queued (nonce 5, fee 100)
        let tx1 = create_tx(&sk, sender, 5, 100);
        assert_eq!(pool.try_add(tx1.clone(), 0), AddResult::AddedQueued);

        // Try replace with same fee - should fail
        let tx2 = create_tx(&sk, sender, 5, 100);
        assert_eq!(pool.try_add(tx2, 0), AddResult::Rejected);

        // Try replace with 109% fee - should fail (need 110%)
        let tx3 = create_tx(&sk, sender, 5, 109);
        assert_eq!(pool.try_add(tx3, 0), AddResult::Rejected);

        // Replace with 110% fee - should succeed
        let tx4 = create_tx(&sk, sender, 5, 110);
        assert_eq!(pool.try_add(tx4.clone(), 0), AddResult::AddedQueued);
        assert_eq!(pool.queued_len(), 1);

        // Old tx should be gone, new one present
        assert!(!pool.contains(&tx1.tx_hash));
        assert!(pool.contains(&tx4.tx_hash));
    }

    #[test]
    fn test_pending_replace_then_remove_non_head() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add nonces 0, 1, 2 to pending
        let tx0 = create_tx(&sk, sender, 0, 50);
        let tx1 = create_tx(&sk, sender, 1, 100);
        let tx2 = create_tx(&sk, sender, 2, 75);
        pool.try_add(tx0.clone(), 0);
        pool.try_add(tx1.clone(), 0);
        pool.try_add(tx2.clone(), 0);
        assert_eq!(pool.sender_heads.len(), 1);
        assert_eq!(pool.pending_len(), 3);

        // Replace non-head (nonce 1) with higher fee
        let tx1_new = create_tx(&sk, sender, 1, 200);
        assert_eq!(pool.try_add(tx1_new.clone(), 0), AddResult::AddedPending);
        assert_eq!(pool.sender_heads.len(), 1); // Still 1 (head unchanged)
        assert_eq!(pool.pending_len(), 3);

        // Remove the replaced tx
        let removed = pool.remove(&tx1_new.tx_hash);
        assert!(removed.is_some());

        // sender_heads should still have 1 entry (head tx0 still exists)
        assert_eq!(pool.sender_heads.len(), 1);
        assert_eq!(pool.pending_len(), 2);

        // Head should still be tx0
        let first = pool.iter_pending().next().unwrap();
        assert_eq!(first.tx_hash, tx0.tx_hash);
    }

    #[test]
    fn test_pending_replace_head_iter_then_remove() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add head tx
        let tx1 = create_tx(&sk, sender, 0, 100);
        pool.try_add(tx1.clone(), 0);

        // Replace head
        let tx2 = create_tx(&sk, sender, 0, 200);
        pool.try_add(tx2.clone(), 0);

        // Iterate - should return the replaced tx with correct fee
        let txs: Vec<_> = pool.iter_pending().collect();
        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].fee, 200);
        assert_eq!(txs[0].tx_hash, tx2.tx_hash);

        // Now remove
        pool.remove(&tx2.tx_hash);
        assert_eq!(pool.sender_heads.len(), 0);
        assert!(pool.iter_pending().next().is_none());
    }

    #[test]
    fn test_pending_multiple_replacements_then_remove() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add head tx (fee 100)
        let tx1 = create_tx(&sk, sender, 0, 100);
        pool.try_add(tx1.clone(), 0);

        // Replace with fee 200
        let tx2 = create_tx(&sk, sender, 0, 200);
        pool.try_add(tx2.clone(), 0);
        assert!(!pool.contains(&tx1.tx_hash));

        // Replace again with fee 400
        let tx3 = create_tx(&sk, sender, 0, 400);
        pool.try_add(tx3.clone(), 0);
        assert!(!pool.contains(&tx2.tx_hash));

        // Replace again with fee 800
        let tx4 = create_tx(&sk, sender, 0, 800);
        pool.try_add(tx4.clone(), 0);
        assert!(!pool.contains(&tx3.tx_hash));

        // Should have exactly 1 entry in sender_heads
        assert_eq!(pool.sender_heads.len(), 1);
        assert_eq!(pool.pending_len(), 1);

        // Remove final tx
        pool.remove(&tx4.tx_hash);

        // sender_heads must be empty
        assert_eq!(pool.sender_heads.len(), 0);
        assert_eq!(pool.pending_len(), 0);
    }

    #[test]
    fn test_pending_replace_head_multi_sender_remove() {
        let mut pool = TransactionPool::new(100);
        let (sk1, sender1) = gen_keypair();
        let (sk2, sender2) = gen_keypair();

        // Add heads for both senders
        let tx1a = create_tx(&sk1, sender1, 0, 50);
        let tx2a = create_tx(&sk2, sender2, 0, 100);
        pool.try_add(tx1a.clone(), 0);
        pool.try_add(tx2a.clone(), 0);
        assert_eq!(pool.sender_heads.len(), 2);

        // Replace sender1's head
        let tx1b = create_tx(&sk1, sender1, 0, 200);
        pool.try_add(tx1b.clone(), 0);
        assert_eq!(pool.sender_heads.len(), 2);

        // Replace sender2's head
        let tx2b = create_tx(&sk2, sender2, 0, 300);
        pool.try_add(tx2b.clone(), 0);
        assert_eq!(pool.sender_heads.len(), 2);

        // Remove sender1's tx
        pool.remove(&tx1b.tx_hash);
        assert_eq!(pool.sender_heads.len(), 1); // Only sender2 remains

        // Remove sender2's tx
        pool.remove(&tx2b.tx_hash);
        assert_eq!(pool.sender_heads.len(), 0); // All cleaned up
    }

    #[test]
    fn test_pending_replace_head_with_subsequent_txs() {
        let mut pool = TransactionPool::new(100);
        let (sk, sender) = gen_keypair();

        // Add nonces 0, 1, 2
        let tx0 = create_tx(&sk, sender, 0, 100);
        let tx1 = create_tx(&sk, sender, 1, 50);
        let tx2 = create_tx(&sk, sender, 2, 25);
        pool.try_add(tx0.clone(), 0);
        pool.try_add(tx1.clone(), 0);
        pool.try_add(tx2.clone(), 0);
        assert_eq!(pool.sender_heads.len(), 1);

        // Replace head (nonce 0)
        let tx0_new = create_tx(&sk, sender, 0, 500);
        pool.try_add(tx0_new.clone(), 0);
        assert_eq!(pool.sender_heads.len(), 1);

        // Remove head - tx1 should become new head
        pool.remove(&tx0_new.tx_hash);
        assert_eq!(pool.sender_heads.len(), 1);
        assert_eq!(pool.pending_len(), 2);

        // Verify tx1 is now head
        let first = pool.iter_pending().next().unwrap();
        assert_eq!(first.tx_hash, tx1.tx_hash);
        assert_eq!(first.nonce, 1);
    }
}
