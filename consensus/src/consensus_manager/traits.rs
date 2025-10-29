use anyhow::Result;

use crate::{
    consensus::ConsensusMessage,
    crypto::aggregated::PeerId,
    state::{block::Block, notarizations::Vote, nullify::Nullify},
};

/// Trait for network communication (P2P layer)
pub trait NetworkSender<const N: usize, const F: usize, const M_SIZE: usize>: Send + Sync {
    /// Broadcasts a consensus message to all peers in the network.
    fn broadcast(&mut self, message: ConsensusMessage<N, F, M_SIZE>) -> Result<()>;

    /// Sends a consensus message to a specific peer in the network.
    fn send_to(&mut self, peer_id: PeerId, message: ConsensusMessage<N, F, M_SIZE>) -> Result<()>;
}

/// Trait for cryptographic operations (signing and verification)
pub trait CryptoProvider: Send + Sync {
    /// Sign a vote for a block
    fn sign_vote(&self, block_hash: [u8; blake3::OUT_LEN], view: u64) -> Result<Vec<u8>>;

    /// Sign a nullify message for a view
    fn sign_nullify(&self, view: u64) -> Result<Vec<u8>>;

    /// Verify a vote signature
    fn verify_vote(&self, vote: &Vote, signature: &[u8]) -> Result<bool>;

    /// Verify a nullify message signature
    fn verify_nullify(&self, msg: &Nullify, signature: &[u8]) -> Result<bool>;
}

/// Application state machine that processes finalized blocks
pub trait ApplicationStateMachine: Send {
    /// Apply a finalized block to the application state
    /// Returns the updated application state
    fn apply_block(&mut self, block: &Block) -> Result<()>;

    /// Get the current state root/hash
    fn state_root(&self) -> [u8; blake3::OUT_LEN];
}
