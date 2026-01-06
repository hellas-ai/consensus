//! Block synchronization protocol.
//!
//! Handles block request/response for nodes that miss blocks.

use crate::error::P2PError;
use crate::message::{
    BlockRequest, BlockResponse, P2PMessage, deserialize_message, serialize_message,
};

/// Encode a block request message.
pub fn encode_block_request<const N: usize, const F: usize, const M_SIZE: usize>(
    view: u64,
    block_hash: Option<[u8; 32]>,
) -> Result<Vec<u8>, P2PError> {
    let msg: P2PMessage<N, F, M_SIZE> = P2PMessage::BlockRequest(BlockRequest { view, block_hash });
    Ok(serialize_message(&msg)?)
}

/// Encode a block response message.
pub fn encode_block_response<const N: usize, const F: usize, const M_SIZE: usize>(
    response: BlockResponse,
) -> Result<Vec<u8>, P2PError> {
    let msg: P2PMessage<N, F, M_SIZE> = P2PMessage::BlockResponse(response);
    Ok(serialize_message(&msg)?)
}

/// Decode a block request from raw bytes.
pub fn decode_block_request<const N: usize, const F: usize, const M_SIZE: usize>(
    bytes: &[u8],
) -> Result<BlockRequest, P2PError> {
    let msg = deserialize_message::<N, F, M_SIZE>(bytes)?;
    match msg {
        P2PMessage::BlockRequest(req) => Ok(req),
        _ => Err(P2PError::MessageType("Expected BlockRequest".to_string())),
    }
}

/// Decode a block response from raw bytes.
pub fn decode_block_response<const N: usize, const F: usize, const M_SIZE: usize>(
    bytes: &[u8],
) -> Result<BlockResponse, P2PError> {
    let msg = deserialize_message::<N, F, M_SIZE>(bytes)?;
    match msg {
        P2PMessage::BlockResponse(resp) => Ok(resp),
        _ => Err(P2PError::MessageType("Expected BlockResponse".to_string())),
    }
}
