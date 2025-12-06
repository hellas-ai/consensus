//! Block Validator - Core Validation Logic
//!
//! This module implements the `BlockValidator` which verifies that all transactions
//! in a block are valid against the current account state. It runs on a dedicated
//! OS thread to avoid blocking the consensus engine.
//!
//! ## Validation Steps
//!
//! For each transaction in a block:
//! 1. **Signature Verification**: Verify Ed25519 signature matches sender
//! 2. **Account Existence**: Check sender account exists (except for CreateAccount)
//! 3. **Balance Check**: Verify sender has sufficient balance for amount + fee
//! 4. **Nonce Check**: Verify transaction nonce matches expected account nonce
//! 5. **Duplicate Check**: Ensure no duplicate tx hashes within the block
//!
//! ## State Tracking
//!
//! The validator uses `PendingStateReader` to get account state that includes:
//! - Finalized state from DB (l-notarized blocks)
//! - Pending state from m-notarized blocks (not yet finalized)
//!
//! This allows validating blocks that build on m-notarized (but not yet
//! l-notarized) blocks, which is essential for the Minimmit BFT protocol.

use std::collections::{HashMap, HashSet};

use crate::state::{
    address::Address,
    block::Block,
    transaction::{Transaction, TransactionInstruction},
};

use super::{PendingStateReader, types::*};

/// Block validator that verifies transactions against account state.
///
/// The validator is designed to be used from a dedicated thread, taking
/// read-only access to the database for account lookups.
pub struct BlockValidator {
    /// Lock-free read access to finalized + pending account state
    pending_state: PendingStateReader,
}

impl BlockValidator {
    /// Creates a new block validator with access to the current chain state.
    pub fn new(pending_state: PendingStateReader) -> Self {
        Self { pending_state }
    }

    /// Validates all transactions in a block against the current account state.
    ///
    /// Returns a `StateDiff` if all transactions are valid, which can be applied
    /// atomically when the block is finalized. Returns a list of errors if any
    /// transaction is invalid.
    ///
    /// # Arguments
    /// * `block` - The block to validate
    ///
    /// # Returns
    /// * `Ok(StateDiff)` - All transactions valid, state changes pre-computed
    /// * `Err(Vec<ValidationError>)` - One or more transactions invalid
    pub fn validate_block(&self, block: &Block) -> ValidationResult<StateDiff> {
        let mut errors = Vec::new();
        let mut state_diff = StateDiff::new();

        // Track seen tx hashes within this block for duplicate detection
        let mut seen_tx_hashes = HashSet::new();

        // Local account state cache: starts from the DB and is updated as we process transactions.
        // This handles multiple transactions from the same sender in a single block.
        let mut local_accounts = HashMap::<Address, LocalAccountState>::new();

        for (tx_index, tx) in block.transactions.iter().enumerate() {
            // 1. Check for duplicate transactions hashes in same block
            if !seen_tx_hashes.insert(tx.tx_hash) {
                errors.push(ValidationError::DuplicateTransaction {
                    tx_index,
                    tx_hash: tx.tx_hash,
                });
                continue;
            }

            // 2. Verify Ed25519 signature
            if !tx.verify() {
                errors.push(ValidationError::InvalidSignature {
                    tx_index,
                    tx_hash: tx.tx_hash,
                });
                continue;
            }

            // 3. Validate transaction based on its instruction type
            match &tx.instruction {
                TransactionInstruction::Transfer { recipient, amount } => self.validate_transfer(
                    tx_index,
                    tx,
                    recipient,
                    amount,
                    &mut local_accounts,
                    &mut state_diff,
                    &mut errors,
                ),
                TransactionInstruction::CreateAccount { address } => self.validate_create_account(
                    tx_index,
                    tx,
                    address,
                    &mut local_accounts,
                    &mut state_diff,
                    &mut errors,
                ),
                TransactionInstruction::Burn { address, amount } => self.validate_burn(
                    tx_index,
                    tx,
                    address,
                    amount,
                    &mut local_accounts,
                    &mut state_diff,
                    &mut errors,
                ),
                TransactionInstruction::Mint { recipient, amount } => self.validate_mint(
                    tx_index,
                    tx,
                    recipient,
                    amount,
                    &mut local_accounts,
                    &mut state_diff,
                    &mut errors,
                ),
            }
        }

        if errors.is_empty() {
            Ok(state_diff)
        } else {
            Err(errors)
        }
    }

    /// Validates a transfer transaction.
    #[allow(clippy::too_many_arguments)]
    fn validate_transfer(
        &self,
        tx_index: usize,
        tx: &Transaction,
        recipient: &Address,
        amount: &u64,
        local_accounts: &mut HashMap<Address, LocalAccountState>,
        state_diff: &mut StateDiff,
        errors: &mut Vec<ValidationError>,
    ) {
        // Get or fetch sender account from local cache or DB
        let sender_state = match self.get_or_fetch_account(&tx.sender, local_accounts) {
            Some(state) => state,
            None => {
                errors.push(ValidationError::AccountNotFound {
                    tx_index,
                    address: tx.sender,
                });
                return;
            }
        };

        // Check nonce
        if tx.nonce != sender_state.nonce {
            errors.push(ValidationError::InvalidNonce {
                tx_index,
                address: tx.sender,
                expected: sender_state.nonce,
                got: tx.nonce,
            });
            return;
        }

        // Check balance (amount + fee)
        let total_debit = match amount.checked_add(tx.fee) {
            Some(total) => total,
            None => {
                errors.push(ValidationError::BalanceOverflow {
                    tx_index,
                    address: tx.sender,
                });
                return;
            }
        };

        if sender_state.balance < total_debit {
            errors.push(ValidationError::InsufficientBalance {
                tx_index,
                address: tx.sender,
                required: total_debit,
                available: sender_state.balance,
            });
            return;
        }

        // At this point, the transaction is valid. Update local state and state diff.
        {
            let sender_state = local_accounts.get_mut(&tx.sender).unwrap();
            sender_state.balance -= total_debit;
            sender_state.nonce += 1;
        }

        // Credit recipient account (may not exist yet, in which case we implicitly create it)
        let recipient_state = self.get_or_create_account(recipient, local_accounts);
        recipient_state.balance = recipient_state.balance.saturating_add(*amount);
        let recipient_nonce = recipient_state.nonce;

        // Re-fetch sender nonce for state_diff (borrow is short-lived)
        let sender_nonce = local_accounts.get(&tx.sender).unwrap().nonce;

        // Record in state_diff
        state_diff.add_balance_change(tx.sender, -(total_debit as i128), sender_nonce);
        state_diff.add_balance_change(*recipient, *amount as i128, recipient_nonce);
        state_diff.add_collected_fees(tx.fee);
    }

    /// Validates a mint transaction.
    ///
    /// For testnet, minting is permission-less and free (no fee). In production, minting
    /// would require either initial token allocation or staking/minting tokens.
    #[allow(clippy::too_many_arguments)]
    fn validate_mint(
        &self,
        tx_index: usize,
        tx: &Transaction,
        recipient: &Address,
        amount: &u64,
        local_accounts: &mut HashMap<Address, LocalAccountState>,
        state_diff: &mut StateDiff,
        errors: &mut Vec<ValidationError>,
    ) {
        // For testnet: anyone can mint
        // Update sender nonce if sender account exists
        if let Some(sender_state) = self.get_or_fetch_account(&tx.sender, local_accounts) {
            // Check nonce
            if tx.nonce != sender_state.nonce {
                errors.push(ValidationError::InvalidNonce {
                    tx_index,
                    address: tx.sender,
                    expected: sender_state.nonce,
                    got: tx.nonce,
                });
                return;
            }

            // Update sender nonce
            let sender_state = local_accounts.get_mut(&tx.sender).unwrap();
            sender_state.nonce += 1;
            state_diff.add_balance_change(tx.sender, 0, sender_state.nonce);
        }

        // Credit recipient
        let recipient_state = self.get_or_create_account(recipient, local_accounts);
        let new_balance = recipient_state.balance.saturating_add(*amount);
        recipient_state.balance = new_balance;

        state_diff.add_balance_change(*recipient, *amount as i128, recipient_state.nonce);
    }

    /// Validates a create account transaction.
    fn validate_create_account(
        &self,
        tx_index: usize,
        tx: &Transaction,
        address: &Address,
        local_accounts: &mut HashMap<Address, LocalAccountState>,
        state_diff: &mut StateDiff,
        errors: &mut Vec<ValidationError>,
    ) {
        // Check if account already exists
        if self.account_exists(address, local_accounts) {
            errors.push(ValidationError::AccountAlreadyExists {
                tx_index,
                address: *address,
            });
            return;
        }

        // Get sender account to deduct fee and update nonce
        let sender_state = match self.get_or_fetch_account(&tx.sender, local_accounts) {
            Some(state) => state,
            None => {
                errors.push(ValidationError::AccountNotFound {
                    tx_index,
                    address: tx.sender,
                });
                return;
            }
        };

        // Check nonce
        if tx.nonce != sender_state.nonce {
            errors.push(ValidationError::InvalidNonce {
                tx_index,
                address: tx.sender,
                expected: sender_state.nonce,
                got: tx.nonce,
            });
            return;
        }

        // Deduct fee and update nonce
        if sender_state.balance < tx.fee {
            errors.push(ValidationError::InsufficientBalance {
                tx_index,
                address: tx.sender,
                required: tx.fee,
                available: sender_state.balance,
            });
            return;
        }

        let sender_new_nonce = {
            let sender_state = local_accounts.get_mut(&tx.sender).unwrap();
            sender_state.balance -= tx.fee;
            sender_state.nonce += 1;
            sender_state.nonce
        };

        // Create new account with initial balance 0
        local_accounts.insert(
            *address,
            LocalAccountState {
                balance: 0,
                nonce: 0,
            },
        );

        // Record in state_diff
        state_diff.add_balance_change(tx.sender, -(tx.fee as i128), sender_new_nonce);
        state_diff.add_created_account(*address, 0);
        state_diff.add_collected_fees(tx.fee);
    }

    /// Validates a burn transaction.
    #[allow(clippy::too_many_arguments)]
    fn validate_burn(
        &self,
        tx_index: usize,
        tx: &Transaction,
        address: &Address,
        amount: &u64,
        local_accounts: &mut HashMap<Address, LocalAccountState>,
        state_diff: &mut StateDiff,
        errors: &mut Vec<ValidationError>,
    ) {
        // Get account to burn from
        let account_state = match self.get_or_fetch_account(address, local_accounts) {
            Some(state) => state,
            None => {
                errors.push(ValidationError::AccountNotFound {
                    tx_index,
                    address: *address,
                });
                return;
            }
        };

        // Check nonce
        if tx.nonce != account_state.nonce {
            errors.push(ValidationError::InvalidNonce {
                tx_index,
                address: *address,
                expected: account_state.nonce,
                got: tx.nonce,
            });
            return;
        }

        // Check balance
        let total_debit = match amount.checked_add(tx.fee) {
            Some(total) => total,
            None => {
                errors.push(ValidationError::BalanceOverflow {
                    tx_index,
                    address: *address,
                });
                return;
            }
        };

        if account_state.balance < total_debit {
            errors.push(ValidationError::InsufficientBalance {
                tx_index,
                address: *address,
                required: total_debit,
                available: account_state.balance,
            });
            return;
        }

        // Debit and update nonce
        let account_state = local_accounts.get_mut(address).unwrap();
        account_state.balance -= total_debit;
        account_state.nonce += 1;

        // Record in state_diff
        state_diff.add_balance_change(*address, -(total_debit as i128), account_state.nonce);
        state_diff.add_collected_fees(tx.fee);
    }

    /// Gets an account from local cache or fetches from the DB.
    /// Returns None if account doesn't exist.
    fn get_or_fetch_account<'a>(
        &self,
        address: &Address,
        local_accounts: &'a mut HashMap<Address, LocalAccountState>,
    ) -> Option<&'a LocalAccountState> {
        if !local_accounts.contains_key(address) {
            // Fetch from pending state (includes DB + m-notarized blocks)
            match self.pending_state.get_account(address) {
                Some(account_state) => {
                    local_accounts.insert(
                        *address,
                        LocalAccountState {
                            balance: account_state.balance,
                            nonce: account_state.nonce,
                        },
                    );
                }
                None => return None,
            }
        }

        local_accounts.get(address)
    }

    // Gets an account from local cache, fetches from DB, or creates a new empty one.
    fn get_or_create_account<'a>(
        &self,
        address: &Address,
        local_accounts: &'a mut HashMap<Address, LocalAccountState>,
    ) -> &'a mut LocalAccountState {
        if !local_accounts.contains_key(address) {
            // Try pending state first (includes DB + m-notarized blocks)
            let state = match self.pending_state.get_account(address) {
                Some(account_state) => LocalAccountState {
                    balance: account_state.balance,
                    nonce: account_state.nonce,
                },
                None => {
                    // Implicit account creation (like Ethereum)
                    LocalAccountState {
                        balance: 0,
                        nonce: 0,
                    }
                }
            };
            local_accounts.insert(*address, state);
        }
        local_accounts.get_mut(address).unwrap()
    }

    /// Checks if an account exists in local cache or pending state.
    fn account_exists(
        &self,
        address: &Address,
        local_accounts: &HashMap<Address, LocalAccountState>,
    ) -> bool {
        local_accounts.contains_key(address) || self.pending_state.get_account(address).is_some()
    }
}

/// Local account state during validation.
///
/// This is a mutable working copy that starts from the DB state and is
/// updated as transactions are processed within a single block.
#[derive(Debug, Clone)]
struct LocalAccountState {
    /// Current balance (after processing previous txs in this block)
    balance: u64,
    /// Current nonce (after processing previous txs in this block)
    nonce: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::crypto::transaction_crypto::TxSecretKey;
    use crate::state::account::Account;
    use crate::state::transaction::Transaction;
    use crate::storage::store::ConsensusStore;
    use crate::validation::PendingStateWriter;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn temp_db_path() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("validator_test_{}.redb", rand::random::<u64>()));
        p
    }

    fn setup_validator() -> (BlockValidator, PendingStateWriter, Arc<ConsensusStore>) {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);
        let validator = BlockValidator::new(reader);
        (validator, writer, store)
    }

    fn gen_keypair() -> (TxSecretKey, Address) {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let addr = Address::from_public_key(&pk);
        (sk, addr)
    }

    #[test]
    fn validate_empty_block() {
        let (validator, _writer, _store) = setup_validator();

        let block = Block::genesis(
            0,
            crate::crypto::aggregated::BlsSecretKey::generate(&mut rand::thread_rng())
                .sign(b"genesis"),
        );

        let result = validator.validate_block(&block);
        assert!(result.is_ok());

        let diff = result.unwrap();
        assert_eq!(diff.num_updates(), 0);
        assert_eq!(diff.num_created(), 0);
        assert_eq!(diff.total_fees, 0);
    }

    #[test]
    fn validate_transfer_insufficient_balance() {
        let (validator, _writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, recipient_addr) = gen_keypair();

        // Create sender account with 100 balance
        let sender_pk = sk.public_key();
        let account = Account::new(sender_pk, 100, 0);
        store.put_account(&account).unwrap();

        // Try to transfer 200 (more than balance)
        let tx = Transaction::new_transfer(sender_addr, recipient_addr, 200, 0, 10, &sk);

        let block = create_test_block(vec![tx]);
        let result = validator.validate_block(&block);

        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert_eq!(errors.len(), 1);
        matches!(errors[0], ValidationError::InsufficientBalance { .. });
    }

    #[test]
    fn validate_transfer_invalid_nonce() {
        let (validator, _writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, recipient_addr) = gen_keypair();

        // Create sender account with nonce 5
        let sender_pk = sk.public_key();
        let account = Account::new(sender_pk, 1000, 5);
        store.put_account(&account).unwrap();

        // Try to use nonce 0 (should be 5)
        let tx = Transaction::new_transfer(sender_addr, recipient_addr, 100, 0, 10, &sk);

        let block = create_test_block(vec![tx]);
        let result = validator.validate_block(&block);

        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert_eq!(errors.len(), 1);
        matches!(
            errors[0],
            ValidationError::InvalidNonce {
                expected: 5,
                got: 0,
                ..
            }
        );
    }

    #[test]
    fn validate_multiple_transfers_from_same_sender() {
        let (validator, _writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, recipient_addr) = gen_keypair();

        // Create sender account with 1000 balance
        let sender_pk = sk.public_key();
        let account = Account::new(sender_pk, 1000, 0);
        store.put_account(&account).unwrap();

        // Two transfers from same sender, correct nonces
        let tx1 = Transaction::new_transfer(sender_addr, recipient_addr, 100, 0, 10, &sk);
        let tx2 = Transaction::new_transfer(sender_addr, recipient_addr, 200, 1, 10, &sk);

        let block = create_test_block(vec![tx1, tx2]);
        let result = validator.validate_block(&block);

        assert!(result.is_ok());
        let diff = result.unwrap();

        // Sender should have: 1000 - 100 - 10 - 200 - 10 = 680
        let sender_update = diff.updates.get(&sender_addr).unwrap();
        assert_eq!(sender_update.balance_delta, -320); // -(100+10+200+10)
        assert_eq!(sender_update.new_nonce, 2);

        // Recipient should have: 0 + 100 + 200 = 300
        let recipient_update = diff.updates.get(&recipient_addr).unwrap();
        assert_eq!(recipient_update.balance_delta, 300);

        assert_eq!(diff.total_fees, 20);
    }

    #[test]
    fn validate_duplicate_transaction_in_block() {
        let (validator, _writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, recipient_addr) = gen_keypair();

        let sender_pk = sk.public_key();
        let account = Account::new(sender_pk, 1000, 0);
        store.put_account(&account).unwrap();

        // Same transaction twice
        let tx = Transaction::new_transfer(sender_addr, recipient_addr, 100, 0, 10, &sk);
        let block = create_test_block(vec![tx.clone(), tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            errors[0],
            ValidationError::DuplicateTransaction { tx_index: 1, .. }
        ));
    }

    #[test]
    fn validate_transfer_account_not_found() {
        let (validator, _writer, _store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, recipient_addr) = gen_keypair();

        // Sender account doesn't exist
        let tx = Transaction::new_transfer(sender_addr, recipient_addr, 100, 0, 10, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(matches!(errors[0], ValidationError::AccountNotFound { .. }));
    }

    #[test]
    fn validate_transfer_to_self() {
        let (validator, _writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();

        let sender_pk = sk.public_key();
        let account = Account::new(sender_pk, 1000, 0);
        store.put_account(&account).unwrap();

        // Transfer to self
        let tx = Transaction::new_transfer(sender_addr, sender_addr, 100, 0, 10, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_ok());

        let diff = result.unwrap();
        // Net change: -100 (amount) - 10 (fee) + 100 (received) = -10 (just fee)
        let update = diff.updates.get(&sender_addr).unwrap();
        assert_eq!(update.balance_delta, -10);
    }

    #[test]
    fn validate_transfer_implicit_recipient_creation() {
        let (validator, _writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, recipient_addr) = gen_keypair();

        let sender_pk = sk.public_key();
        let account = Account::new(sender_pk, 1000, 0);
        store.put_account(&account).unwrap();

        // Recipient doesn't exist - should be implicitly created
        let tx = Transaction::new_transfer(sender_addr, recipient_addr, 100, 0, 10, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_ok());

        let diff = result.unwrap();
        let recipient_update = diff.updates.get(&recipient_addr).unwrap();
        assert_eq!(recipient_update.balance_delta, 100);
        assert_eq!(recipient_update.new_nonce, 0); // Implicitly created, nonce 0
    }

    #[test]
    fn validate_create_account_success() {
        let (validator, _writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, new_addr) = gen_keypair();

        let sender_pk = sk.public_key();
        let account = Account::new(sender_pk, 1000, 0);
        store.put_account(&account).unwrap();

        let tx = Transaction::new_create_account(sender_addr, new_addr, 0, 10, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_ok());

        let diff = result.unwrap();
        assert_eq!(diff.num_created(), 1);
        assert!(diff.created_accounts.iter().any(|c| c.address == new_addr));
        assert_eq!(diff.total_fees, 10);
    }

    #[test]
    fn validate_create_account_already_exists() {
        let (validator, _writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (existing_sk, existing_addr) = gen_keypair();

        let sender_pk = sk.public_key();
        store
            .put_account(&Account::new(sender_pk, 1000, 0))
            .unwrap();

        // Pre-create the account
        let existing_pk = existing_sk.public_key();
        store
            .put_account(&Account::new(existing_pk, 500, 0))
            .unwrap();

        let tx = Transaction::new_create_account(sender_addr, existing_addr, 0, 10, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(matches!(
            errors[0],
            ValidationError::AccountAlreadyExists { .. }
        ));
    }

    #[test]
    fn validate_create_account_insufficient_fee() {
        let (validator, _writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, new_addr) = gen_keypair();

        let sender_pk = sk.public_key();
        let account = Account::new(sender_pk, 5, 0); // Only 5 balance
        store.put_account(&account).unwrap();

        let tx = Transaction::new_create_account(sender_addr, new_addr, 0, 10, &sk); // 10 fee
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(matches!(
            errors[0],
            ValidationError::InsufficientBalance { .. }
        ));
    }

    #[test]
    fn validate_mint_to_new_account() {
        let (validator, _writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, recipient_addr) = gen_keypair();

        let sender_pk = sk.public_key();
        store.put_account(&Account::new(sender_pk, 0, 0)).unwrap();

        let tx = Transaction::new_mint(sender_addr, recipient_addr, 5000, 0, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_ok());

        let diff = result.unwrap();
        let recipient_update = diff.updates.get(&recipient_addr).unwrap();
        assert_eq!(recipient_update.balance_delta, 5000);
    }

    #[test]
    fn validate_mint_invalid_nonce() {
        let (validator, _writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, recipient_addr) = gen_keypair();

        let sender_pk = sk.public_key();
        store.put_account(&Account::new(sender_pk, 0, 5)).unwrap(); // nonce 5

        let tx = Transaction::new_mint(sender_addr, recipient_addr, 5000, 0, &sk); // nonce 0
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err()[0],
            ValidationError::InvalidNonce { .. }
        ));
    }

    #[test]
    fn validate_burn_success() {
        let (validator, _writer, store) = setup_validator();
        let (sk, addr) = gen_keypair();

        let pk = sk.public_key();
        store.put_account(&Account::new(pk, 1000, 0)).unwrap();

        let tx = Transaction::new_burn(addr, addr, 500, 0, 10, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_ok());

        let diff = result.unwrap();
        let update = diff.updates.get(&addr).unwrap();
        assert_eq!(update.balance_delta, -510); // 500 burned + 10 fee
    }

    #[test]
    fn validate_burn_insufficient_balance() {
        let (validator, _writer, store) = setup_validator();
        let (sk, addr) = gen_keypair();

        let pk = sk.public_key();
        store.put_account(&Account::new(pk, 100, 0)).unwrap();

        let tx = Transaction::new_burn(addr, addr, 500, 0, 10, &sk); // 510 total, only 100 available
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err()[0],
            ValidationError::InsufficientBalance { .. }
        ));
    }

    #[test]
    fn validate_chain_of_pending_views() {
        let (validator, mut writer, _store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, recipient_addr) = gen_keypair();

        // View 1: Create account with 1000
        let mut diff1 = StateDiff::new();
        diff1.add_created_account(sender_addr, 1000);
        writer.add_m_notarized_diff(1, diff1);

        // View 2: Spend 200 (nonce 0 -> 1)
        let mut diff2 = StateDiff::new();
        diff2.add_balance_change(sender_addr, -200, 1);
        writer.add_m_notarized_diff(2, diff2);

        // Now validate a block at view 3 that spends from current state (800 balance, nonce 1)
        let tx = Transaction::new_transfer(sender_addr, recipient_addr, 100, 1, 10, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_ok(), "Should see accumulated pending state");

        let diff = result.unwrap();
        assert_eq!(diff.updates.get(&sender_addr).unwrap().balance_delta, -110);
        assert_eq!(diff.updates.get(&sender_addr).unwrap().new_nonce, 2);
    }

    #[test]
    fn validate_balance_overflow_protection() {
        let (validator, _writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, recipient_addr) = gen_keypair();

        let sender_pk = sk.public_key();
        store
            .put_account(&Account::new(sender_pk, u64::MAX, 0))
            .unwrap();

        // Try to transfer with fee that would overflow
        let tx = Transaction::new_transfer(sender_addr, recipient_addr, u64::MAX, 0, 1, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err()[0],
            ValidationError::BalanceOverflow { .. }
        ));
    }

    #[test]
    fn validate_multiple_errors_reported() {
        let (validator, _writer, _store) = setup_validator();
        let (sk1, addr1) = gen_keypair();
        let (sk2, addr2) = gen_keypair();
        let (_, recipient) = gen_keypair();

        // Both accounts don't exist
        let tx1 = Transaction::new_transfer(addr1, recipient, 100, 0, 10, &sk1);
        let tx2 = Transaction::new_transfer(addr2, recipient, 200, 0, 10, &sk2);
        let block = create_test_block(vec![tx1, tx2]);

        let result = validator.validate_block(&block);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert_eq!(errors.len(), 2); // Both transactions fail
    }

    fn create_test_block(transactions: Vec<Transaction>) -> Block {
        use crate::crypto::aggregated::BlsSecretKey;
        let sk = BlsSecretKey::generate(&mut rand::thread_rng());
        let sig = sk.sign(b"test block");

        Block::new(1, 0, [0u8; 32], transactions, 0, sig, false, 1)
    }

    #[test]
    fn validate_against_pending_state() {
        let (validator, mut writer, _store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, recipient_addr) = gen_keypair();

        // Create account via pending state (simulating m-notarized block)
        let mut diff = StateDiff::new();
        diff.add_created_account(sender_addr, 1000);
        writer.add_m_notarized_diff(1, diff);

        // Now validate a block that spends from the pending account
        let tx = Transaction::new_transfer(sender_addr, recipient_addr, 100, 0, 10, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_ok(), "Should validate against pending state");

        let diff = result.unwrap();
        assert_eq!(diff.updates.get(&sender_addr).unwrap().balance_delta, -110);
    }

    #[test]
    fn validate_burn_invalid_nonce() {
        let (validator, _writer, store) = setup_validator();
        let (sk, addr) = gen_keypair();

        let pk = sk.public_key();
        store.put_account(&Account::new(pk, 1000, 5)).unwrap(); // nonce 5

        let tx = Transaction::new_burn(addr, addr, 100, 0, 10, &sk); // nonce 0
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err()[0],
            ValidationError::InvalidNonce {
                expected: 5,
                got: 0,
                ..
            }
        ));
    }

    #[test]
    fn validate_burn_account_not_found() {
        let (validator, _writer, _store) = setup_validator();
        let (sk, addr) = gen_keypair();

        // Account doesn't exist
        let tx = Transaction::new_burn(addr, addr, 100, 0, 10, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err()[0],
            ValidationError::AccountNotFound { .. }
        ));
    }

    #[test]
    fn validate_burn_overflow_protection() {
        let (validator, _writer, store) = setup_validator();
        let (sk, addr) = gen_keypair();

        let pk = sk.public_key();
        store.put_account(&Account::new(pk, u64::MAX, 0)).unwrap();

        // amount + fee would overflow
        let tx = Transaction::new_burn(addr, addr, u64::MAX, 0, 1, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err()[0],
            ValidationError::BalanceOverflow { .. }
        ));
    }

    #[test]
    fn validate_create_account_invalid_nonce() {
        let (validator, _writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, new_addr) = gen_keypair();

        let sender_pk = sk.public_key();
        store
            .put_account(&Account::new(sender_pk, 1000, 5))
            .unwrap(); // nonce 5

        let tx = Transaction::new_create_account(sender_addr, new_addr, 0, 10, &sk); // nonce 0
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err()[0],
            ValidationError::InvalidNonce {
                expected: 5,
                got: 0,
                ..
            }
        ));
    }

    #[test]
    fn validate_create_account_sender_not_found() {
        let (validator, _writer, _store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, new_addr) = gen_keypair();

        // Sender doesn't exist
        let tx = Transaction::new_create_account(sender_addr, new_addr, 0, 10, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err()[0],
            ValidationError::AccountNotFound { .. }
        ));
    }

    #[test]
    fn validate_mint_to_existing_account() {
        let (validator, _writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (recipient_sk, recipient_addr) = gen_keypair();

        let sender_pk = sk.public_key();
        store.put_account(&Account::new(sender_pk, 0, 0)).unwrap();

        // Pre-create recipient with 500 balance
        let recipient_pk = recipient_sk.public_key();
        store
            .put_account(&Account::new(recipient_pk, 500, 0))
            .unwrap();

        let tx = Transaction::new_mint(sender_addr, recipient_addr, 1000, 0, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_ok());

        let diff = result.unwrap();
        // Recipient should have +1000 delta (500 + 1000 = 1500 total)
        let recipient_update = diff.updates.get(&recipient_addr).unwrap();
        assert_eq!(recipient_update.balance_delta, 1000);
    }

    #[test]
    fn validate_mint_sender_not_found_still_works() {
        let (validator, _writer, _store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, recipient_addr) = gen_keypair();

        // Sender doesn't exist - mint should still work (permissionless on testnet)
        let tx = Transaction::new_mint(sender_addr, recipient_addr, 5000, 0, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_ok());

        let diff = result.unwrap();
        assert_eq!(
            diff.updates.get(&recipient_addr).unwrap().balance_delta,
            5000
        );
    }

    #[test]
    fn validate_mixed_transaction_types() {
        let (validator, _writer, store) = setup_validator();
        let (sk1, addr1) = gen_keypair();
        let (sk2, addr2) = gen_keypair();
        let (_, addr3) = gen_keypair();

        let pk1 = sk1.public_key();
        let pk2 = sk2.public_key();
        store.put_account(&Account::new(pk1, 1000, 0)).unwrap();
        store.put_account(&Account::new(pk2, 500, 0)).unwrap();

        // Mix of transfer, mint, burn
        let tx1 = Transaction::new_transfer(addr1, addr3, 100, 0, 10, &sk1);
        let tx2 = Transaction::new_mint(addr2, addr3, 200, 0, &sk2);
        let tx3 = Transaction::new_burn(addr1, addr1, 50, 1, 5, &sk1);

        let block = create_test_block(vec![tx1, tx2, tx3]);
        let result = validator.validate_block(&block);

        assert!(result.is_ok());
        let diff = result.unwrap();

        // addr1: -110 (transfer) - 55 (burn) = -165
        assert_eq!(diff.updates.get(&addr1).unwrap().balance_delta, -165);
        // addr3: +100 (transfer) + 200 (mint) = +300
        assert_eq!(diff.updates.get(&addr3).unwrap().balance_delta, 300);
    }

    #[test]
    fn validate_implicit_then_explicit_create_fails() {
        let (validator, _writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, new_addr) = gen_keypair();

        let sender_pk = sk.public_key();
        store
            .put_account(&Account::new(sender_pk, 1000, 0))
            .unwrap();

        // First implicitly create via transfer, then try explicit CreateAccount
        let tx1 = Transaction::new_transfer(sender_addr, new_addr, 100, 0, 10, &sk);
        let tx2 = Transaction::new_create_account(sender_addr, new_addr, 1, 10, &sk);

        let block = create_test_block(vec![tx1, tx2]);
        let result = validator.validate_block(&block);

        // Should fail - account already exists (implicitly created)
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err()[0],
            ValidationError::AccountAlreadyExists { .. }
        ));
    }

    #[test]
    fn validate_multiple_mints_to_same_recipient() {
        let (validator, _writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, recipient_addr) = gen_keypair();

        let sender_pk = sk.public_key();
        store.put_account(&Account::new(sender_pk, 0, 0)).unwrap();

        let tx1 = Transaction::new_mint(sender_addr, recipient_addr, 100, 0, &sk);
        let tx2 = Transaction::new_mint(sender_addr, recipient_addr, 200, 1, &sk);
        let tx3 = Transaction::new_mint(sender_addr, recipient_addr, 300, 2, &sk);

        let block = create_test_block(vec![tx1, tx2, tx3]);
        let result = validator.validate_block(&block);

        assert!(result.is_ok());
        let diff = result.unwrap();
        assert_eq!(
            diff.updates.get(&recipient_addr).unwrap().balance_delta,
            600
        );
    }

    #[test]
    fn validate_zero_amount_transfer() {
        let (validator, _writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, recipient_addr) = gen_keypair();

        let sender_pk = sk.public_key();
        store.put_account(&Account::new(sender_pk, 100, 0)).unwrap();

        // Zero amount transfer (only fee)
        let tx = Transaction::new_transfer(sender_addr, recipient_addr, 0, 0, 10, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_ok());

        let diff = result.unwrap();
        assert_eq!(diff.updates.get(&sender_addr).unwrap().balance_delta, -10); // just fee
        assert_eq!(diff.updates.get(&recipient_addr).unwrap().balance_delta, 0);
    }

    #[test]
    fn validate_finalized_with_pending_updates() {
        let (validator, mut writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, recipient_addr) = gen_keypair();

        // Account exists in DB with 500 balance, nonce 0
        let sender_pk = sk.public_key();
        store.put_account(&Account::new(sender_pk, 500, 0)).unwrap();

        // Pending state adds 300 more and updates nonce to 1
        let mut pending_diff = StateDiff::new();
        pending_diff.add_balance_change(sender_addr, 300, 1);
        writer.add_m_notarized_diff(1, pending_diff);

        // Now validate with nonce 1 (pending) and balance 800 (500 + 300)
        let tx = Transaction::new_transfer(sender_addr, recipient_addr, 700, 1, 10, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_ok(), "Should see finalized + pending state");

        let diff = result.unwrap();
        assert_eq!(diff.updates.get(&sender_addr).unwrap().balance_delta, -710);
        assert_eq!(diff.updates.get(&sender_addr).unwrap().new_nonce, 2);
    }

    #[test]
    fn validate_account_created_in_pending_used_for_create_account() {
        let (validator, mut writer, store) = setup_validator();
        let (sk, sender_addr) = gen_keypair();
        let (_, new_addr) = gen_keypair();

        // Sender funded in DB
        let sender_pk = sk.public_key();
        store
            .put_account(&Account::new(sender_pk, 1000, 0))
            .unwrap();

        // new_addr was created in a pending block
        let mut pending_diff = StateDiff::new();
        pending_diff.add_created_account(new_addr, 0);
        writer.add_m_notarized_diff(1, pending_diff);

        // Try to CreateAccount for same address - should fail
        let tx = Transaction::new_create_account(sender_addr, new_addr, 0, 10, &sk);
        let block = create_test_block(vec![tx]);

        let result = validator.validate_block(&block);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err()[0],
            ValidationError::AccountAlreadyExists { .. }
        ));
    }
}
