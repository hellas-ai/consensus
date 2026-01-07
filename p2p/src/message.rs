//! Wire protocol message types for P2P communication.
//!
//! Messages are serialized using commonware-codec for efficient encoding.

use consensus::{consensus::ConsensusMessage, state::transaction::Transaction};
use rkyv::{Archive, Deserialize, Serialize};

/// Channel identifiers for different message types.
pub mod channels {
    use commonware_p2p::Channel;

    /// Channel for consensus protocol messages.
    pub const CONSENSUS: Channel = 0;

    /// Channel for transaction gossip.
    pub const TRANSACTIONS: Channel = 1;

    /// Channel for block sync requests/responses.
    pub const BLOCK_SYNC: Channel = 2;
}

// TODO: Do we want to add identity request/response ? For metadata sharing across nodes ?
// This might be useful if we allow dynamic membership.

/// Wire protocol message envelope.
///
/// All messages sent over the P2P network are wrapped in this enum.
#[derive(Clone, Debug, Archive, Deserialize, Serialize)]
pub enum P2PMessage<const N: usize, const F: usize, const M_SIZE: usize> {
    /// Consensus protocol messages (block proposals, votes, notarizations).
    Consensus(ConsensusMessage<N, F, M_SIZE>),

    /// Single transaction gossip.
    Transaction(Transaction),

    /// Batch of transactions (for efficiency).
    TransactionBatch(Vec<Transaction>),

    /// Heartbeat ping with timestamp.
    Ping(u64),

    /// Heartbeat pong response.
    Pong(u64),

    /// Request a block by view number.
    BlockRequest(BlockRequest),

    /// Response containing a requested block.
    BlockResponse(BlockResponse),
}

/// Request a missing block from a peer.
#[derive(Clone, Debug, Archive, Deserialize, Serialize)]
pub struct BlockRequest {
    /// View number of the requested block.
    pub view: u64,
    /// Hash of the requested block (for verification).
    pub block_hash: Option<[u8; 32]>,
}

/// Response containing a requested block.
#[derive(Clone, Debug, Archive, Deserialize, Serialize)]
pub enum BlockResponse {
    /// Successfully found the requested block.
    Found {
        /// The block bytes (serialized).
        block_bytes: Vec<u8>,
    },
    /// Block not found for the requested view.
    NotFound {
        /// The view that was requested.
        view: u64,
    },
    /// Block hash mismatch.
    HashMismatch {
        /// The view that was requested.
        view: u64,
        /// The hash the peer actually has.
        actual_hash: Option<[u8; 32]>,
    },
}

/// Serialize a message to bytes using rkyv.
pub fn serialize_message<const N: usize, const F: usize, const M_SIZE: usize>(
    msg: &P2PMessage<N, F, M_SIZE>,
) -> Result<Vec<u8>, rkyv::rancor::Error> {
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(msg)?;
    Ok(bytes.to_vec())
}

/// Deserialize a message from bytes using rkyv.
pub fn deserialize_message<const N: usize, const F: usize, const M_SIZE: usize>(
    bytes: &[u8],
) -> Result<P2PMessage<N, F, M_SIZE>, anyhow::Error> {
    let archived =
        rkyv::access::<rkyv::Archived<P2PMessage<N, F, M_SIZE>>, rkyv::rancor::Error>(bytes)
            .map_err(|e| anyhow::anyhow!("Failed to access archived: {:?}", e))?;
    let msg: P2PMessage<N, F, M_SIZE> = rkyv::deserialize::<_, rkyv::rancor::Error>(archived)
        .map_err(|e| anyhow::anyhow!("Deserialization failed: {:?}", e))?;
    Ok(msg)
}
