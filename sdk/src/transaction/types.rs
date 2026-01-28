//! Transaction types and status

use crate::types::Hash;

/// A signed transaction ready for submission.
#[derive(Debug, Clone)]
pub struct SignedTransaction {
    /// Raw serialized bytes for submission via gRPC.
    pub(crate) bytes: Vec<u8>,
    /// Transaction hash for tracking.
    pub tx_hash: Hash,
}

impl SignedTransaction {
    /// Get the transaction bytes for submission.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Get the transaction hash.
    pub fn hash(&self) -> &Hash {
        &self.tx_hash
    }
}

/// Transaction status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxStatus {
    /// Transaction is in the mempool, not yet in a block.
    Pending,
    /// Transaction is in an M-notarized block (not finalized).
    MNotarized {
        /// Block hash containing the transaction.
        block_hash: Hash,
        /// Block height.
        block_height: u64,
    },
    /// Transaction is finalized.
    Finalized {
        /// Block hash containing the transaction.
        block_hash: Hash,
        /// Block height.
        block_height: u64,
    },
    /// Transaction not found.
    NotFound,
}

/// Receipt returned after transaction is finalized.
#[derive(Debug, Clone)]
pub struct TxReceipt {
    /// Transaction hash.
    pub tx_hash: Hash,
    /// Block hash where transaction was included.
    pub block_hash: Hash,
    /// Block height.
    pub block_height: u64,
    /// Index within the block.
    pub tx_index: u32,
}

/// Transaction information returned from queries.
#[derive(Debug, Clone)]
pub struct TxInfo {
    /// Transaction hash.
    pub tx_hash: Hash,
    /// Sender address.
    pub sender: crate::types::Address,
    /// Transaction type description.
    pub tx_type: TxType,
    /// Transaction nonce.
    pub nonce: u64,
    /// Transaction fee.
    pub fee: u64,
    /// Timestamp.
    pub timestamp: u64,
}

/// Transaction type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxType {
    /// Transfer tokens.
    Transfer {
        to: crate::types::Address,
        amount: u64,
    },
    /// Mint tokens (testnet).
    Mint {
        to: crate::types::Address,
        amount: u64,
    },
    /// Burn tokens.
    Burn {
        address: crate::types::Address,
        amount: u64,
    },
    /// Create account.
    CreateAccount { address: crate::types::Address },
    /// Unknown transaction type.
    Unknown,
}
