//! Transaction gossip protocol.
//!
//! This module provides utilities for encoding transaction messages
//! for gossip over the P2P network.

use consensus::state::transaction::Transaction;

use crate::error::P2PError;
use crate::message::{P2PMessage, serialize_message};

/// Encode a single transaction for gossip.
pub fn encode_transaction<const N: usize, const F: usize, const M_SIZE: usize>(
    tx: Transaction,
) -> Result<Vec<u8>, P2PError> {
    let msg: P2PMessage<N, F, M_SIZE> = P2PMessage::Transaction(tx);
    Ok(serialize_message(&msg)?)
}

/// Encode a batch of transactions for gossip.
pub fn encode_transaction_batch<const N: usize, const F: usize, const M_SIZE: usize>(
    txs: Vec<Transaction>,
) -> Result<Vec<u8>, P2PError> {
    let msg: P2PMessage<N, F, M_SIZE> = P2PMessage::TransactionBatch(txs);
    Ok(serialize_message(&msg)?)
}
