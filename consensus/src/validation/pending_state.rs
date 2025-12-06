//! Pending State Manager - Lock-Free Implementation
//!
//! Uses arc-swap for lock-free reads from the validation thread while the
//! consensus thread can atomically publish new snapshots.
//!
//! ## Architecture
//!
//! - `PendingStateSnapshot`: Immutable snapshot of pending state (never mutated)
//! - `PendingStateWriter`: Owned by consensus thread, creates and publishes snapshots
//! - `PendingStateReader`: Cloned to validation thread, reads snapshots lock-free

use std::{collections::BTreeMap, sync::Arc};

use arc_swap::{ArcSwap, Guard};

use crate::state::account::Account;
use crate::state::address::Address;
use crate::storage::store::ConsensusStore;

use super::types::{AccountUpdate, StateDiff};

// TODO: Check if we can use Rc<StateDiff> instead of Arc<StateDiff>

/// Immutable snapshot of pending state.
///
/// This is never mutated after creation. When state changes, a new
/// snapshot is created and atomically swapped in.
#[derive(Clone)]
pub struct PendingStateSnapshot {
    /// Pending state diffs from m-notarized blocks, ordered by view.
    pending_diffs: BTreeMap<u64, Arc<StateDiff>>,
    /// The highest l-notarized (finalized) view.
    last_finalized_view: u64,
    /// Reference to finalized state in DB.
    store: Arc<ConsensusStore>,
}

impl PendingStateSnapshot {
    /// Creates an empty snapshot.
    pub fn new(store: Arc<ConsensusStore>, last_finalized_view: u64) -> Self {
        Self {
            pending_diffs: BTreeMap::new(),
            last_finalized_view,
            store,
        }
    }
    /// Gets the effective account state by overlaying pending diffs on finalized state.
    pub fn get_account(&self, address: &Address) -> Option<AccountState> {
        // Start with finalized DB state (may be None)
        let mut state = self.fetch_from_db(address);

        // Single forward pass: incrementally update state as we encounter changes
        for diff in self.pending_diffs.values() {
            // Explicit creation - creates/resets the account
            if let Some(created) = diff.created_accounts.iter().find(|c| c.address == *address) {
                state = Some(AccountState {
                    balance: created.initial_balance,
                    nonce: 0,
                    exists: true,
                });
            }

            // Apply update if present
            if let Some(update) = diff.updates.get(address) {
                if let Some(ref mut s) = state {
                    // Account exists - apply the update
                    s.apply_update(update);
                } else if update.balance_delta > 0 {
                    // Implicit creation: receiving funds creates the account
                    state = Some(AccountState {
                        balance: update.balance_delta as u64,
                        nonce: update.new_nonce,
                        exists: true,
                    });
                }
                // Negative update without base state is invalid - skip
            }
        }

        state
    }

    /// Returns the last finalized view.
    pub fn last_finalized_view(&self) -> u64 {
        self.last_finalized_view
    }

    /// Returns the number of pending views.
    pub fn pending_count(&self) -> usize {
        self.pending_diffs.len()
    }

    fn fetch_from_db(&self, address: &Address) -> Option<AccountState> {
        let public_key = address.to_public_key()?;
        let account = self.store.get_account(&public_key).ok()??;
        Some(AccountState {
            balance: account.balance,
            nonce: account.nonce,
            exists: true,
        })
    }
}

/// Writer handle for the consensus thread.
///
/// Owns the mutable state and publishes immutable snapshots.
/// NOT Clone - only one writer should exist.
pub struct PendingStateWriter {
    /// The shared snapshot that readers access.
    shared: Arc<ArcSwap<PendingStateSnapshot>>,
    /// Mutable working copy (only accessed by this writer).
    pending_diffs: BTreeMap<u64, Arc<StateDiff>>,
    /// Last finalized view.
    last_finalized_view: u64,
    /// DB reference for new snapshots.
    store: Arc<ConsensusStore>,
}

impl PendingStateWriter {
    /// Creates a new writer and returns a reader handle.
    pub fn new(store: Arc<ConsensusStore>, last_finalized_view: u64) -> (Self, PendingStateReader) {
        let initial_snapshot = PendingStateSnapshot::new(Arc::clone(&store), last_finalized_view);
        let shared = Arc::new(ArcSwap::new(Arc::new(initial_snapshot)));

        let writer = Self {
            shared: Arc::clone(&shared),
            pending_diffs: BTreeMap::new(),
            last_finalized_view,
            store,
        };

        let reader = PendingStateReader { shared };

        (writer, reader)
    }

    /// Called when a block is m-notarized: adds its StateDiff as pending.
    pub fn add_m_notarized_diff(&mut self, view: u64, diff: StateDiff) {
        if view > self.last_finalized_view {
            self.pending_diffs.insert(view, Arc::new(diff));
            self.publish_snapshot();
        }
    }

    /// Called when a block is l-notarized: removes finalized diffs and applies to DB.
    pub fn finalize_up_to(&mut self, view: u64) -> anyhow::Result<()> {
        // Process and remove views <= finalized view (no allocation)
        while let Some((&v, _)) = self.pending_diffs.first_key_value() {
            if v > view {
                break;
            }
            // `pop_first()` is guaranteed to succeed here
            let (_, diff) = self.pending_diffs.pop_first().unwrap();
            self.apply_diff_to_db(&diff)?;
        }

        if view > self.last_finalized_view {
            self.last_finalized_view = view;
        }

        self.publish_snapshot();
        Ok(())
    }

    /// Called when a view is nullified: removes its pending diff.
    pub fn remove_nullified_view(&mut self, view: u64) {
        let mut changed = false;
        if self.pending_diffs.remove(&view).is_some() {
            changed = true;
        }
        if changed {
            self.publish_snapshot();
        }
    }

    /// Removes all pending diffs after the given view (for reorgs).
    pub fn rollback_after(&mut self, view: u64) {
        let before_count = self.pending_diffs.len();
        self.pending_diffs.retain(|v, _| *v <= view);
        if self.pending_diffs.len() != before_count {
            self.publish_snapshot();
        }
    }

    /// Creates and publishes a new immutable snapshot.
    fn publish_snapshot(&self) {
        let snapshot = PendingStateSnapshot {
            pending_diffs: self.pending_diffs.clone(),
            last_finalized_view: self.last_finalized_view,
            store: Arc::clone(&self.store),
        };
        // Atomic swap - readers see old or new, never partial state
        self.shared.store(Arc::new(snapshot));
    }

    /// Applies a state diff to the database.
    fn apply_diff_to_db(&self, diff: &StateDiff) -> anyhow::Result<()> {
        // Create new accounts
        for created in &diff.created_accounts {
            if let Some(pk) = created.address.to_public_key() {
                let account = Account::new(pk, created.initial_balance, 0);
                self.store.put_account(&account)?;
            }
        }

        // Apply balance/nonce updates
        for (address, update) in &diff.updates {
            if let Some(pk) = address.to_public_key() {
                // Treat missing table as "account not found"
                let existing = self.store.get_account(&pk).ok().flatten();

                if let Some(mut account) = existing {
                    let new_balance = if update.balance_delta >= 0 {
                        account.balance.saturating_add(update.balance_delta as u64)
                    } else {
                        account
                            .balance
                            .saturating_sub((-update.balance_delta) as u64)
                    };
                    account.balance = new_balance;
                    account.nonce = update.new_nonce;
                    self.store.put_account(&account)?;
                } else if update.balance_delta > 0 {
                    // Implicitly created account (received funds)
                    let account = Account::new(pk, update.balance_delta as u64, update.new_nonce);
                    self.store.put_account(&account)?;
                }
            }
        }

        Ok(())
    }
}

/// Reader handle for the validation thread.
///
/// Clone-able and can be sent across threads.
/// All reads are lock-free.
#[derive(Clone)]
pub struct PendingStateReader {
    shared: Arc<ArcSwap<PendingStateSnapshot>>,
}

impl PendingStateReader {
    /// Gets the current snapshot (lock-free).
    ///
    /// The returned Guard keeps the snapshot alive even if a new
    /// one is published while you're using it.
    #[inline]
    pub fn load(&self) -> Guard<Arc<PendingStateSnapshot>> {
        self.shared.load()
    }

    /// Gets account state from the current snapshot (lock-free).
    #[inline]
    pub fn get_account(&self, address: &Address) -> Option<AccountState> {
        self.load().get_account(address)
    }
}

/// Effective account state computed from finalized + pending state.
#[derive(Debug, Clone)]
pub struct AccountState {
    pub balance: u64,
    pub nonce: u64,
    pub exists: bool,
}

impl AccountState {
    fn apply_update(&mut self, update: &AccountUpdate) {
        if update.balance_delta >= 0 {
            self.balance = self.balance.saturating_add(update.balance_delta as u64);
        } else {
            self.balance = self.balance.saturating_sub((-update.balance_delta) as u64);
        }
        self.nonce = update.new_nonce;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::transaction_crypto::TxSecretKey;
    use std::path::PathBuf;

    fn temp_db_path() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("pending_state_test_{}.redb", rand::random::<u64>()));
        p
    }

    #[test]
    fn lock_free_read_while_writing() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // Reader sees empty state
        assert!(reader.get_account(&addr).is_none());

        // Writer adds pending creation
        let mut diff = StateDiff::new();
        diff.add_created_account(addr, 1000);
        writer.add_m_notarized_diff(1, diff);

        // Reader now sees the account (lock-free!)
        let state = reader.get_account(&addr).unwrap();
        assert_eq!(state.balance, 1000);
    }

    #[test]
    fn snapshot_isolation() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // Add initial state
        let mut diff = StateDiff::new();
        diff.add_created_account(addr, 1000);
        writer.add_m_notarized_diff(1, diff);

        // Take a snapshot
        let snapshot = reader.load();
        assert_eq!(snapshot.get_account(&addr).unwrap().balance, 1000);

        // Writer updates state
        let mut diff2 = StateDiff::new();
        diff2.add_balance_change(addr, -500, 1);
        writer.add_m_notarized_diff(2, diff2);

        // Old snapshot still sees 1000
        assert_eq!(snapshot.get_account(&addr).unwrap().balance, 1000);

        // New load sees 500
        assert_eq!(reader.get_account(&addr).unwrap().balance, 500);
    }

    #[test]
    fn get_account_from_db_only() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());

        // Create account directly in DB
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let addr = Address::from_public_key(&pk);
        let account = Account::new(pk, 5000, 10);
        store.put_account(&account).unwrap();

        let (_writer, reader) = PendingStateWriter::new(store, 0);

        // Should see DB state
        let state = reader.get_account(&addr).unwrap();
        assert_eq!(state.balance, 5000);
        assert_eq!(state.nonce, 10);
        assert!(state.exists);
    }

    #[test]
    fn get_account_explicit_creation_no_db() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // Explicit creation with initial balance
        let mut diff = StateDiff::new();
        diff.add_created_account(addr, 2000);
        writer.add_m_notarized_diff(1, diff);

        let state = reader.get_account(&addr).unwrap();
        assert_eq!(state.balance, 2000);
        assert_eq!(state.nonce, 0);
        assert!(state.exists);
    }

    #[test]
    fn get_account_implicit_creation_via_transfer() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // No explicit creation, just a positive balance delta (transfer received)
        let mut diff = StateDiff::new();
        diff.add_balance_change(addr, 3000, 0);
        writer.add_m_notarized_diff(1, diff);

        // Should implicitly create account
        let state = reader.get_account(&addr).unwrap();
        assert_eq!(state.balance, 3000);
        assert_eq!(state.nonce, 0);
        assert!(state.exists);
    }

    #[test]
    fn get_account_db_with_pending_updates() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());

        // Create account in DB
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let addr = Address::from_public_key(&pk);
        let account = Account::new(pk, 1000, 5);
        store.put_account(&account).unwrap();

        let (mut writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        // Add pending update (spend 200, nonce becomes 6)
        let mut diff = StateDiff::new();
        diff.add_balance_change(addr, -200, 6);
        writer.add_m_notarized_diff(1, diff);

        let state = reader.get_account(&addr).unwrap();
        assert_eq!(state.balance, 800);
        assert_eq!(state.nonce, 6);
    }

    #[test]
    fn get_account_creation_and_update_same_view() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // Same view: create account with 0 balance, then receive 500
        let mut diff = StateDiff::new();
        diff.add_created_account(addr, 0);
        diff.add_balance_change(addr, 500, 0); // Received transfer
        writer.add_m_notarized_diff(1, diff);

        let state = reader.get_account(&addr).unwrap();
        assert_eq!(state.balance, 500); // 0 + 500
        assert_eq!(state.nonce, 0);
    }

    #[test]
    fn get_account_creation_then_updates_later_views() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // View 1: create with 1000
        let mut diff1 = StateDiff::new();
        diff1.add_created_account(addr, 1000);
        writer.add_m_notarized_diff(1, diff1);

        // View 2: receive 500
        let mut diff2 = StateDiff::new();
        diff2.add_balance_change(addr, 500, 0);
        writer.add_m_notarized_diff(2, diff2);

        // View 3: spend 300, nonce=1
        let mut diff3 = StateDiff::new();
        diff3.add_balance_change(addr, -300, 1);
        writer.add_m_notarized_diff(3, diff3);

        let state = reader.get_account(&addr).unwrap();
        assert_eq!(state.balance, 1200); // 1000 + 500 - 300
        assert_eq!(state.nonce, 1);
    }

    #[test]
    fn get_account_implicit_creation_then_more_updates() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // View 1: implicit creation via transfer
        let mut diff1 = StateDiff::new();
        diff1.add_balance_change(addr, 1000, 0);
        writer.add_m_notarized_diff(1, diff1);

        // View 2: receive more
        let mut diff2 = StateDiff::new();
        diff2.add_balance_change(addr, 200, 0);
        writer.add_m_notarized_diff(2, diff2);

        // View 3: spend some (first outgoing tx, nonce=1)
        let mut diff3 = StateDiff::new();
        diff3.add_balance_change(addr, -150, 1);
        writer.add_m_notarized_diff(3, diff3);

        let state = reader.get_account(&addr).unwrap();
        assert_eq!(state.balance, 1050); // 1000 + 200 - 150
        assert_eq!(state.nonce, 1);
    }

    #[test]
    fn get_account_multiple_updates_accumulate() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());

        // Start with DB account
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let addr = Address::from_public_key(&pk);
        let account = Account::new(pk, 10000, 0);
        store.put_account(&account).unwrap();

        let (mut writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        // 5 sequential transactions
        for i in 1..=5 {
            let mut diff = StateDiff::new();
            diff.add_balance_change(addr, -100, i);
            writer.add_m_notarized_diff(i, diff);
        }

        let state = reader.get_account(&addr).unwrap();
        assert_eq!(state.balance, 9500); // 10000 - 500
        assert_eq!(state.nonce, 5);
    }

    #[test]
    fn get_account_nonexistent_returns_none() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (_writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // No DB entry, no pending state
        assert!(reader.get_account(&addr).is_none());
    }

    #[test]
    fn get_account_negative_delta_without_base_ignored() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // Negative delta for nonexistent account (invalid state, should be ignored)
        let mut diff = StateDiff::new();
        diff.add_balance_change(addr, -500, 1);
        writer.add_m_notarized_diff(1, diff);

        // Should still return None (can't implicitly create with negative balance)
        assert!(reader.get_account(&addr).is_none());
    }

    #[test]
    fn get_account_finalization_removes_pending() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let addr = Address::from_public_key(&pk);

        // Add pending creation
        let mut diff1 = StateDiff::new();
        diff1.add_created_account(addr, 1000);
        writer.add_m_notarized_diff(1, diff1);

        // Add pending update
        let mut diff2 = StateDiff::new();
        diff2.add_balance_change(addr, 500, 0);
        writer.add_m_notarized_diff(2, diff2);

        assert_eq!(reader.get_account(&addr).unwrap().balance, 1500);

        // Finalize view 1 (creation goes to DB)
        writer.finalize_up_to(1).unwrap();

        // Should still see correct state (DB + remaining pending)
        let state = reader.get_account(&addr).unwrap();
        assert_eq!(state.balance, 1500); // DB now has 1000, pending has +500

        // Finalize view 2
        writer.finalize_up_to(2).unwrap();

        // All in DB now
        let db_account = store.get_account(&pk).unwrap().unwrap();
        assert_eq!(db_account.balance, 1500);

        // Reader still sees correct state
        assert_eq!(reader.get_account(&addr).unwrap().balance, 1500);
    }

    #[test]
    fn account_exists_all_cases() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());

        // Account in DB
        let sk1 = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk1 = sk1.public_key();
        let addr1 = Address::from_public_key(&pk1);
        store.put_account(&Account::new(pk1, 100, 0)).unwrap();

        let (mut writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        // Account explicitly created
        let sk2 = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr2 = Address::from_public_key(&sk2.public_key());
        let mut diff1 = StateDiff::new();
        diff1.add_created_account(addr2, 200);
        writer.add_m_notarized_diff(1, diff1);

        // Account implicitly created
        let sk3 = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr3 = Address::from_public_key(&sk3.public_key());
        let mut diff2 = StateDiff::new();
        diff2.add_balance_change(addr3, 300, 0);
        writer.add_m_notarized_diff(2, diff2);

        // Nonexistent
        let sk4 = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr4 = Address::from_public_key(&sk4.public_key());

        assert!(reader.get_account(&addr1).is_some()); // DB
        assert!(reader.get_account(&addr2).is_some()); // Explicit creation
        assert!(reader.get_account(&addr3).is_some()); // Implicit creation
        assert!(reader.get_account(&addr4).is_none()); // Nonexistent
    }

    #[test]
    fn add_m_notarized_diff_ignores_old_views() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        // Start with last_finalized_view = 5
        let (mut writer, reader) = PendingStateWriter::new(store, 5);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // Try to add diff for view 3 (< 5) - should be ignored
        let mut diff = StateDiff::new();
        diff.add_created_account(addr, 1000);
        writer.add_m_notarized_diff(3, diff.clone());

        assert!(reader.get_account(&addr).is_none());

        // Try view 5 (== last_finalized) - should also be ignored
        writer.add_m_notarized_diff(5, diff.clone());
        assert!(reader.get_account(&addr).is_none());

        // View 6 (> 5) - should work
        writer.add_m_notarized_diff(6, diff);
        assert!(reader.get_account(&addr).is_some());
    }

    #[test]
    fn add_m_notarized_diff_multiple_views() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // Add diffs for views 1, 2, 3
        let mut diff1 = StateDiff::new();
        diff1.add_created_account(addr, 1000);
        writer.add_m_notarized_diff(1, diff1);

        let mut diff2 = StateDiff::new();
        diff2.add_balance_change(addr, 500, 0);
        writer.add_m_notarized_diff(2, diff2);

        let mut diff3 = StateDiff::new();
        diff3.add_balance_change(addr, -200, 1);
        writer.add_m_notarized_diff(3, diff3);

        // Verify accumulated state
        let state = reader.get_account(&addr).unwrap();
        assert_eq!(state.balance, 1300); // 1000 + 500 - 200
        assert_eq!(state.nonce, 1);

        // Verify pending count
        let snapshot = reader.load();
        assert_eq!(snapshot.pending_count(), 3);
    }

    #[test]
    fn finalize_up_to_single_view() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let addr = Address::from_public_key(&pk);

        // Add pending diff
        let mut diff = StateDiff::new();
        diff.add_created_account(addr, 1000);
        writer.add_m_notarized_diff(1, diff);

        assert_eq!(reader.load().pending_count(), 1);

        // Finalize
        writer.finalize_up_to(1).unwrap();

        // Pending should be empty
        assert_eq!(reader.load().pending_count(), 0);

        // But account should still be visible (now from DB)
        assert_eq!(reader.get_account(&addr).unwrap().balance, 1000);

        // Verify it's in DB
        let db_account = store.get_account(&pk).unwrap().unwrap();
        assert_eq!(db_account.balance, 1000);
    }

    #[test]
    fn finalize_up_to_multiple_views() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let addr = Address::from_public_key(&pk);

        // Add 5 pending diffs
        let mut diff1 = StateDiff::new();
        diff1.add_created_account(addr, 1000);
        writer.add_m_notarized_diff(1, diff1);

        for i in 2..=5 {
            let mut diff = StateDiff::new();
            diff.add_balance_change(addr, 100, i - 1);
            writer.add_m_notarized_diff(i, diff);
        }

        assert_eq!(reader.load().pending_count(), 5);
        assert_eq!(reader.get_account(&addr).unwrap().balance, 1400); // 1000 + 4*100

        // Finalize views 1-3
        writer.finalize_up_to(3).unwrap();

        // Only views 4, 5 remain pending
        assert_eq!(reader.load().pending_count(), 2);

        // Account state still correct
        assert_eq!(reader.get_account(&addr).unwrap().balance, 1400);

        // DB should have state up to view 3
        let db_account = store.get_account(&pk).unwrap().unwrap();
        assert_eq!(db_account.balance, 1200); // 1000 + 2*100
        assert_eq!(db_account.nonce, 2);
    }

    #[test]
    fn finalize_up_to_with_gaps() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let addr = Address::from_public_key(&pk);

        // Add diffs for views 1, 3, 5 (gaps at 2, 4)
        let mut diff1 = StateDiff::new();
        diff1.add_created_account(addr, 1000);
        writer.add_m_notarized_diff(1, diff1);

        let mut diff3 = StateDiff::new();
        diff3.add_balance_change(addr, 300, 1);
        writer.add_m_notarized_diff(3, diff3);

        let mut diff5 = StateDiff::new();
        diff5.add_balance_change(addr, 500, 2);
        writer.add_m_notarized_diff(5, diff5);

        // Finalize up to view 4 (should finalize 1 and 3)
        writer.finalize_up_to(4).unwrap();

        // Only view 5 remains
        assert_eq!(reader.load().pending_count(), 1);

        // DB has views 1 and 3
        let db_account = store.get_account(&pk).unwrap().unwrap();
        assert_eq!(db_account.balance, 1300);
        assert_eq!(db_account.nonce, 1);
    }

    #[test]
    fn finalize_applies_implicit_creation_to_db() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, _reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let addr = Address::from_public_key(&pk);

        // Implicit creation via positive balance delta
        let mut diff = StateDiff::new();
        diff.add_balance_change(addr, 5000, 0);
        writer.add_m_notarized_diff(1, diff);

        // Finalize
        writer.finalize_up_to(1).unwrap();

        // Should be in DB
        let db_account = store.get_account(&pk).unwrap().unwrap();
        assert_eq!(db_account.balance, 5000);
        assert_eq!(db_account.nonce, 0);
    }

    #[test]
    fn remove_nullified_view_removes_existing() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // Add diffs for views 1, 2, 3
        for i in 1..=3 {
            let mut diff = StateDiff::new();
            diff.add_created_account(addr, i * 1000);
            writer.add_m_notarized_diff(i, diff);
        }

        assert_eq!(reader.load().pending_count(), 3);

        // Remove view 2
        writer.remove_nullified_view(2);

        assert_eq!(reader.load().pending_count(), 2);

        // View 3's creation should be the final state (overwrites view 1's creation)
        assert_eq!(reader.get_account(&addr).unwrap().balance, 3000);
    }

    #[test]
    fn remove_nullified_view_noop_for_nonexistent() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        let mut diff = StateDiff::new();
        diff.add_created_account(addr, 1000);
        writer.add_m_notarized_diff(1, diff);

        // Remove non-existent view
        writer.remove_nullified_view(999);

        // Nothing changed
        assert_eq!(reader.load().pending_count(), 1);
        assert_eq!(reader.get_account(&addr).unwrap().balance, 1000);
    }

    #[test]
    fn rollback_after_removes_later_views() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // Add diffs for views 1-5
        let mut diff1 = StateDiff::new();
        diff1.add_created_account(addr, 1000);
        writer.add_m_notarized_diff(1, diff1);

        for i in 2..=5 {
            let mut diff = StateDiff::new();
            diff.add_balance_change(addr, 100, i - 1);
            writer.add_m_notarized_diff(i, diff);
        }

        assert_eq!(reader.load().pending_count(), 5);
        assert_eq!(reader.get_account(&addr).unwrap().balance, 1400);

        // Rollback after view 2 (removes views 3, 4, 5)
        writer.rollback_after(2);

        assert_eq!(reader.load().pending_count(), 2);
        assert_eq!(reader.get_account(&addr).unwrap().balance, 1100); // 1000 + 100
    }

    #[test]
    fn rollback_after_keeps_equal_view() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        let mut diff = StateDiff::new();
        diff.add_created_account(addr, 1000);
        writer.add_m_notarized_diff(5, diff);

        // Rollback after view 5 - should keep view 5
        writer.rollback_after(5);

        assert_eq!(reader.load().pending_count(), 1);
        assert_eq!(reader.get_account(&addr).unwrap().balance, 1000);
    }

    #[test]
    fn rollback_after_noop_when_nothing_to_remove() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        let mut diff = StateDiff::new();
        diff.add_created_account(addr, 1000);
        writer.add_m_notarized_diff(1, diff);

        // Rollback after view 100 - nothing to remove
        writer.rollback_after(100);

        assert_eq!(reader.load().pending_count(), 1);
    }

    #[test]
    fn arc_statediff_sharing_efficiency() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // Add a diff
        let mut diff = StateDiff::new();
        diff.add_created_account(addr, 1000);
        writer.add_m_notarized_diff(1, diff);

        // Take two snapshots
        let snapshot1 = reader.load();
        let snapshot2 = reader.load();

        // Both should see the same data
        assert_eq!(snapshot1.get_account(&addr).unwrap().balance, 1000);
        assert_eq!(snapshot2.get_account(&addr).unwrap().balance, 1000);

        // Arc::ptr_eq would verify same underlying data, but we can't access
        // the inner Arc<StateDiff> directly. The test verifies correctness at least.
    }

    #[test]
    fn finalize_updates_last_finalized_view() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        let mut diff = StateDiff::new();
        diff.add_created_account(addr, 1000);
        writer.add_m_notarized_diff(5, diff);

        assert_eq!(reader.load().last_finalized_view(), 0);

        writer.finalize_up_to(5).unwrap();

        assert_eq!(reader.load().last_finalized_view(), 5);

        // New diffs at view <= 5 should be ignored
        let mut diff2 = StateDiff::new();
        diff2.add_balance_change(addr, 500, 1);
        writer.add_m_notarized_diff(5, diff2);

        // Should still just have the original 1000 from DB
        assert_eq!(reader.get_account(&addr).unwrap().balance, 1000);
    }

    #[test]
    fn empty_statediff_handling() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        // Add empty diff
        let diff = StateDiff::new();
        writer.add_m_notarized_diff(1, diff);

        // Should have 1 pending view (even if empty)
        assert_eq!(reader.load().pending_count(), 1);

        // Finalize empty diff - should not error
        writer.finalize_up_to(1).unwrap();

        assert_eq!(reader.load().pending_count(), 0);
    }

    #[test]
    fn multiple_accounts_in_same_diff() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let sk1 = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let sk2 = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let sk3 = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr1 = Address::from_public_key(&sk1.public_key());
        let addr2 = Address::from_public_key(&sk2.public_key());
        let addr3 = Address::from_public_key(&sk3.public_key());

        // Single diff with multiple accounts
        let mut diff = StateDiff::new();
        diff.add_created_account(addr1, 1000);
        diff.add_created_account(addr2, 2000);
        diff.add_balance_change(addr3, 3000, 0); // Implicit creation
        writer.add_m_notarized_diff(1, diff);

        assert_eq!(reader.get_account(&addr1).unwrap().balance, 1000);
        assert_eq!(reader.get_account(&addr2).unwrap().balance, 2000);
        assert_eq!(reader.get_account(&addr3).unwrap().balance, 3000);
    }

    #[test]
    fn balance_overflow_protection() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // Create with max balance
        let mut diff1 = StateDiff::new();
        diff1.add_created_account(addr, u64::MAX);
        writer.add_m_notarized_diff(1, diff1);

        // Try to add more - should saturate, not overflow
        let mut diff2 = StateDiff::new();
        diff2.add_balance_change(addr, 1000, 0);
        writer.add_m_notarized_diff(2, diff2);

        assert_eq!(reader.get_account(&addr).unwrap().balance, u64::MAX);
    }

    #[test]
    fn balance_underflow_protection() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // Create with small balance
        let mut diff1 = StateDiff::new();
        diff1.add_created_account(addr, 100);
        writer.add_m_notarized_diff(1, diff1);

        // Try to subtract more than balance - should saturate to 0
        let mut diff2 = StateDiff::new();
        diff2.add_balance_change(addr, -1000, 1);
        writer.add_m_notarized_diff(2, diff2);

        assert_eq!(reader.get_account(&addr).unwrap().balance, 0);
    }

    #[test]
    fn finalize_with_no_pending_diffs() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        // Finalize when there's nothing pending - should not error
        writer.finalize_up_to(10).unwrap();

        assert_eq!(reader.load().pending_count(), 0);
        assert_eq!(reader.load().last_finalized_view(), 10);
    }

    #[test]
    fn rollback_to_view_zero() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // Add several diffs
        for i in 1..=5 {
            let mut diff = StateDiff::new();
            diff.add_created_account(addr, i * 1000);
            writer.add_m_notarized_diff(i, diff);
        }

        assert_eq!(reader.load().pending_count(), 5);

        // Rollback to 0 - should remove all
        writer.rollback_after(0);

        assert_eq!(reader.load().pending_count(), 0);
        assert!(reader.get_account(&addr).is_none());
    }

    #[test]
    fn cloned_reader_works_correctly() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader1) = PendingStateWriter::new(store, 0);

        // Clone the reader
        let reader2 = reader1.clone();

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        let mut diff = StateDiff::new();
        diff.add_created_account(addr, 1000);
        writer.add_m_notarized_diff(1, diff);

        // Both readers should see the same state
        assert_eq!(reader1.get_account(&addr).unwrap().balance, 1000);
        assert_eq!(reader2.get_account(&addr).unwrap().balance, 1000);
    }

    #[test]
    fn finalize_creation_and_update_same_diff_to_db() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, _reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let addr = Address::from_public_key(&pk);

        // Create account and receive transfer in same block
        let mut diff = StateDiff::new();
        diff.add_created_account(addr, 0);
        diff.add_balance_change(addr, 500, 0);
        writer.add_m_notarized_diff(1, diff);

        // Finalize
        writer.finalize_up_to(1).unwrap();

        // DB should have the combined result
        let db_account = store.get_account(&pk).unwrap().unwrap();
        assert_eq!(db_account.balance, 500);
    }

    #[test]
    fn finalize_updates_existing_db_account() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());

        // Pre-existing account in DB
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let addr = Address::from_public_key(&pk);
        let account = Account::new(pk.clone(), 1000, 5);
        store.put_account(&account).unwrap();

        let (mut writer, _reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        // Add update
        let mut diff = StateDiff::new();
        diff.add_balance_change(addr, -300, 6);
        writer.add_m_notarized_diff(1, diff);

        // Finalize
        writer.finalize_up_to(1).unwrap();

        // DB should be updated
        let db_account = store.get_account(&pk).unwrap().unwrap();
        assert_eq!(db_account.balance, 700);
        assert_eq!(db_account.nonce, 6);
    }

    #[test]
    fn concurrent_read_write_safety() {
        use std::thread;

        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        // Spawn 100 reader threads
        let mut reader_handles = Vec::with_capacity(100);
        for reader_id in 0..100 {
            let reader_clone = reader.clone();
            let addr_clone = addr;

            let handle = thread::spawn(move || {
                let mut last_balance = 0u64;
                let mut read_count = 0u64;
                for _ in 0..100 {
                    if let Some(state) = reader_clone.get_account(&addr_clone) {
                        // Balance should only increase (we're only adding)
                        assert!(
                            state.balance >= last_balance,
                            "Reader {}: balance went from {} to {}",
                            reader_id,
                            last_balance,
                            state.balance
                        );
                        last_balance = state.balance;
                        read_count += 1;
                    }
                    thread::yield_now();
                }
                // Return final observed state
                (reader_id, read_count, reader_clone.get_account(&addr_clone))
            });

            reader_handles.push(handle);
        }

        // Writer adds diffs (creates account with 100, then adds 100 each time)
        // Total: 100 + 49*100 = 5000
        for i in 1..=50 {
            let mut diff = StateDiff::new();
            if i == 1 {
                diff.add_created_account(addr, 100);
            } else {
                diff.add_balance_change(addr, 100, i - 1);
            }
            writer.add_m_notarized_diff(i, diff);
            thread::yield_now();
        }

        // Wait for all readers and verify
        let expected_final = 100 + 49 * 100; // 5000
        let mut total_reads = 0u64;

        for handle in reader_handles {
            let (reader_id, read_count, final_state) = handle.join().unwrap();
            total_reads += read_count;

            // Every reader should see correct final state
            assert_eq!(
                final_state.unwrap().balance,
                expected_final,
                "Reader {} final state incorrect",
                reader_id
            );
        }

        println!(
            "100 readers completed with {} total successful reads, final balance = {}",
            total_reads, expected_final
        );
    }

    #[test]
    fn bench_write_operations() {
        use std::time::Instant;

        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, _reader) = PendingStateWriter::new(store, 0);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        const ITERATIONS: u64 = 1_000;

        // Benchmark add_m_notarized_diff
        let start = Instant::now();
        for i in 1..=ITERATIONS {
            let mut diff = StateDiff::new();
            if i == 1 {
                diff.add_created_account(addr, 1000);
            } else {
                diff.add_balance_change(addr, 100, i - 1);
            }
            writer.add_m_notarized_diff(i, diff);
        }
        let add_elapsed = start.elapsed();

        println!(
            "add_m_notarized_diff: {} iterations in {:?} ({:.2} µs/op)",
            ITERATIONS,
            add_elapsed,
            add_elapsed.as_micros() as f64 / ITERATIONS as f64
        );

        // Benchmark finalize_up_to (finalizing all at once)
        let start = Instant::now();
        writer.finalize_up_to(ITERATIONS).unwrap();
        let finalize_elapsed = start.elapsed();

        println!(
            "finalize_up_to({} views): {:?} ({:.2} µs/view)",
            ITERATIONS,
            finalize_elapsed,
            finalize_elapsed.as_micros() as f64 / ITERATIONS as f64
        );
    }

    #[test]
    fn bench_read_operations() {
        use std::time::Instant;

        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (mut writer, reader) = PendingStateWriter::new(store, 0);

        // Setup: create accounts and pending diffs
        let mut addresses = Vec::with_capacity(100);
        for _ in 0..100 {
            let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
            addresses.push(Address::from_public_key(&sk.public_key()));
        }

        // Add pending diffs for multiple views with multiple accounts
        for view in 1..=10 {
            let mut diff = StateDiff::new();
            for addr in &addresses {
                if view == 1 {
                    diff.add_created_account(*addr, 1000);
                } else {
                    diff.add_balance_change(*addr, 100, view - 1);
                }
            }
            writer.add_m_notarized_diff(view, diff);
        }

        const READ_ITERATIONS: usize = 10_000;

        // Benchmark get_account (via reader)
        let start = Instant::now();
        for i in 0..READ_ITERATIONS {
            let addr = &addresses[i % addresses.len()];
            let _ = reader.get_account(addr);
        }
        let read_elapsed = start.elapsed();

        println!(
            "get_account: {} reads in {:?} ({:.2} µs/read)",
            READ_ITERATIONS,
            read_elapsed,
            read_elapsed.as_micros() as f64 / READ_ITERATIONS as f64
        );

        // Benchmark snapshot load
        let start = Instant::now();
        for _ in 0..READ_ITERATIONS {
            let _ = reader.load();
        }
        let load_elapsed = start.elapsed();

        println!(
            "reader.load(): {} loads in {:?} ({:.2} ns/load)",
            READ_ITERATIONS,
            load_elapsed,
            load_elapsed.as_nanos() as f64 / READ_ITERATIONS as f64
        );
    }

    #[test]
    fn bench_arc_statediff_clone_vs_owned() {
        use std::time::Instant;

        // Simulate a large StateDiff
        let mut large_diff = StateDiff::new();
        for i in 0..1000 {
            let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
            let addr = Address::from_public_key(&sk.public_key());
            large_diff.add_created_account(addr, i * 100);
            large_diff.add_balance_change(addr, 50, 0);
        }

        const ITERATIONS: usize = 1000;

        // Benchmark Arc clone (what we do now)
        let arc_diff = Arc::new(large_diff.clone());
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let _cloned = Arc::clone(&arc_diff);
        }
        let arc_elapsed = start.elapsed();

        // Benchmark StateDiff clone (what we'd do without Arc)
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let _cloned = large_diff.clone();
        }
        let owned_elapsed = start.elapsed();

        println!(
            "Arc<StateDiff> clone: {} iterations in {:?} ({:.2} ns/clone)",
            ITERATIONS,
            arc_elapsed,
            arc_elapsed.as_nanos() as f64 / ITERATIONS as f64
        );
        println!(
            "StateDiff clone: {} iterations in {:?} ({:.2} µs/clone)",
            ITERATIONS,
            owned_elapsed,
            owned_elapsed.as_micros() as f64 / ITERATIONS as f64
        );
        println!(
            "Speedup: {:.0}x",
            owned_elapsed.as_nanos() as f64 / arc_elapsed.as_nanos() as f64
        );
    }
}
