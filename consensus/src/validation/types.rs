//! Block Validation Types
//!
//! This module defines the core types used for block validation in the consensus protocol.
//! The validation service runs on a dedicated OS thread and communicates with the consensus
//! engine via lock-free rtrb ring buffers.

use std::{collections::HashMap, time::Instant};

use crate::state::{address::Address, block::Block};

/// A block that has passed validation and is ready for consensus.
///
/// Contains the original block plus the pre-computed state diff
/// to be applied atomically when the block is finalized.
#[derive(Clone, Debug)]
pub struct ValidatedBlock {
    /// The original block
    pub block: Block,
    /// The state diff to be applied when the block is finalized
    pub state_diff: StateDiff,
    /// The time when the block was validated
    pub validated_at: Instant,
}

impl ValidatedBlock {
    pub fn new(block: Block, state_diff: StateDiff) -> Self {
        Self {
            block,
            state_diff,
            validated_at: Instant::now(),
        }
    }

    /// Return the view of the validated block
    #[inline]
    pub fn view(&self) -> u64 {
        self.block.view()
    }

    /// Return the hash of the validated block
    #[inline]
    pub fn hash(&self) -> [u8; blake3::OUT_LEN] {
        self.block.get_hash()
    }

    /// Return the state diff of the validated block
    #[inline]
    pub fn state_diff(&self) -> &StateDiff {
        &self.state_diff
    }
}

#[derive(Clone, Debug, Default)]
pub struct StateDiff {
    /// Account updates: address -> delta
    pub updates: HashMap<Address, AccountUpdate>,
    /// New accounts created by this block
    pub created_accounts: Vec<NewAccount>,
    /// Total transaction fees collected in this block
    pub total_fees: u64,
}

impl StateDiff {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a balance change for an account
    pub fn add_balance_change(&mut self, address: Address, delta: i128, new_nonce: u64) {
        self.updates
            .entry(address)
            .and_modify(|update| {
                update.balance_delta += delta;
                update.new_nonce = new_nonce;
            })
            .or_insert(AccountUpdate::new(delta, new_nonce));
    }

    /// Records a new account creation
    pub fn add_created_account(&mut self, address: Address, initial_balance: u64) {
        self.created_accounts.push(NewAccount {
            address,
            initial_balance,
        });
    }

    /// Records the total fees collected in this block
    pub fn add_collected_fees(&mut self, fees: u64) {
        self.total_fees += fees;
    }

    /// Returns the number of account updates
    pub fn num_updates(&self) -> usize {
        self.updates.len()
    }

    /// Returns the number of new accounts
    pub fn num_created(&self) -> usize {
        self.created_accounts.len()
    }
}

/// A balance/nonce update for a single account.
#[derive(Debug, Clone)]
pub struct AccountUpdate {
    /// Change in balance. Positive for credits, negative for debits.
    pub balance_delta: i128,
    /// The new nonce after all transactions in this block.
    pub new_nonce: u64,
}

impl AccountUpdate {
    pub fn new(balance_delta: i128, new_nonce: u64) -> Self {
        Self {
            balance_delta,
            new_nonce,
        }
    }
}

/// A new account to be created.
#[derive(Debug, Clone)]
pub struct NewAccount {
    /// The address of the new account
    pub address: Address,
    /// Initial balance (usually 0, or from a mint)
    pub initial_balance: u64,
}

/// Errors that can occur during block validation.
///
/// Each error includes the transaction index for debugging and includes
/// enough context to understand what went wrong.
#[derive(Debug, Clone)]
pub enum ValidationError {
    /// Ed25519 signature verification failed
    InvalidSignature { tx_index: usize, tx_hash: [u8; 32] },

    /// The sender account does not exist in the database
    AccountNotFound { tx_index: usize, address: Address },

    /// Sender has insufficient balance for transfer + fee
    InsufficientBalance {
        tx_index: usize,
        address: Address,
        required: u64,
        available: u64,
    },

    /// Transaction nonce doesn't match the expected account nonce
    InvalidNonce {
        tx_index: usize,
        address: Address,
        expected: u64,
        got: u64,
    },

    /// Duplicate transaction hash within the same block
    DuplicateTransaction { tx_index: usize, tx_hash: [u8; 32] },

    /// Attempt to create an account that already exists
    AccountAlreadyExists { tx_index: usize, address: Address },

    /// Arithmetic overflow during balance calculation
    BalanceOverflow { tx_index: usize, address: Address },
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSignature { tx_index, tx_hash } => {
                write!(
                    f,
                    "Invalid signature for tx {} (hash: {})",
                    tx_index,
                    hex::encode(&tx_hash[..8])
                )
            }
            Self::AccountNotFound { tx_index, address } => {
                write!(f, "Account not found for tx {}: {}", tx_index, address)
            }
            Self::InsufficientBalance {
                tx_index,
                address,
                required,
                available,
            } => {
                write!(
                    f,
                    "Insufficient balance for tx {} ({}): required {}, available {}",
                    tx_index, address, required, available
                )
            }
            Self::InvalidNonce {
                tx_index,
                address,
                expected,
                got,
            } => {
                write!(
                    f,
                    "Invalid nonce for tx {} ({}): expected {}, got {}",
                    tx_index, address, expected, got
                )
            }
            Self::DuplicateTransaction { tx_index, tx_hash } => {
                write!(
                    f,
                    "Duplicate transaction at index {}: {}",
                    tx_index,
                    hex::encode(&tx_hash[..8])
                )
            }
            Self::AccountAlreadyExists { tx_index, address } => {
                write!(f, "Account already exists for tx {}: {}", tx_index, address)
            }
            Self::BalanceOverflow { tx_index, address } => {
                write!(f, "Balance overflow for tx {} ({})", tx_index, address)
            }
        }
    }
}

impl std::error::Error for ValidationError {}

/// Result type alias for validation operations
pub type ValidationResult<T> = Result<T, Vec<ValidationError>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_diff_tracks_balance_changes() {
        let mut diff = StateDiff::new();
        let addr = Address::from_bytes([1u8; 32]);

        diff.add_balance_change(addr, -100, 1);
        diff.add_balance_change(addr, -50, 2);

        let update = diff.updates.get(&addr).unwrap();
        assert_eq!(update.balance_delta, -150);
        assert_eq!(update.new_nonce, 2);
    }

    #[test]
    fn state_diff_tracks_created_accounts() {
        let mut diff = StateDiff::new();
        let addr1 = Address::from_bytes([1u8; 32]);
        let addr2 = Address::from_bytes([2u8; 32]);

        diff.add_created_account(addr1, 0);
        diff.add_created_account(addr2, 1000);

        assert_eq!(diff.num_created(), 2);
        assert_eq!(diff.created_accounts[0].initial_balance, 0);
        assert_eq!(diff.created_accounts[1].initial_balance, 1000);
    }

    #[test]
    fn state_diff_accumulates_fees() {
        let mut diff = StateDiff::new();

        diff.add_collected_fees(100);
        diff.add_collected_fees(50);
        diff.add_collected_fees(25);

        assert_eq!(diff.total_fees, 175);
    }

    #[test]
    fn validation_error_display() {
        let err = ValidationError::InsufficientBalance {
            tx_index: 5,
            address: Address::from_bytes([0xab; 32]),
            required: 1000,
            available: 500,
        };

        let msg = format!("{}", err);
        assert!(msg.contains("Insufficient balance"));
        assert!(msg.contains("tx 5"));
        assert!(msg.contains("1000"));
        assert!(msg.contains("500"));
    }
}
