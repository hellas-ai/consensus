//! Consensus protocol message handler.
//!
//! This module provides message serialization and deserialization utilities
//! for consensus messages. The actual sending/receiving is done through
//! commonware-p2p channels configured at runtime.

use crate::error::P2PError;
use crate::message::{P2PMessage, deserialize_message, serialize_message};

/// Serialize a consensus message to bytes for sending.
pub fn encode_message<const N: usize, const F: usize, const M_SIZE: usize>(
    msg: &P2PMessage<N, F, M_SIZE>,
) -> Result<Vec<u8>, P2PError> {
    Ok(serialize_message(msg)?)
}

/// Deserialize bytes into a consensus message.
pub fn decode_message<const N: usize, const F: usize, const M_SIZE: usize>(
    bytes: &[u8],
) -> Result<P2PMessage<N, F, M_SIZE>, P2PError> {
    Ok(deserialize_message(bytes)?)
}
