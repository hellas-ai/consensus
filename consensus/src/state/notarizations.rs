use rkyv::{Archive, Deserialize, Serialize};

use crate::crypto::aggregated::{AggregatedSignature, BlsPublicKey, BlsSignature, PeerId};
use crate::crypto::conversions::ArkSerdeWrapper;
use crate::state::block::Block;

/// [`Vote`] represents a vote for a given block.
///
/// A vote corresponds to an authenticated block, from a given peer.
#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct Vote {
    /// The hash of the block that is being voted for
    pub block_hash: [u8; blake3::OUT_LEN],
    /// The signature of block by the peer that is voting
    /// for the current block
    #[rkyv(with = ArkSerdeWrapper)]
    pub signature: BlsSignature,
    /// The public key of the peer that is
    /// voting for the current block
    #[rkyv(with = ArkSerdeWrapper)]
    pub peer_id: PeerId,
}

impl Vote {
    pub fn new(
        block_hash: [u8; blake3::OUT_LEN],
        signature: BlsSignature,
        peer_id: PeerId,
    ) -> Self {
        Self {
            block_hash,
            signature,
            peer_id,
        }
    }

    /// Verifies if the block has been successfully signed by its author
    /// Note: this does not verify that the [`PeerId`] matches the public key
    /// of the peer that signed the block. This should be verified by the caller, beforehand.
    pub fn verify(&self, peer_public_key: &BlsPublicKey) -> bool {
        peer_public_key.verify(&self.block_hash, &self.signature)
    }
}

/// [`MNotarization`] represents a (2f + 1)-signature quorum for a given block (also
/// referred to as a M-notarization). An M-notarization is required to progress to the next view,
/// but not to finalize a block. Moreover, a block for view `v` can receive a M-notarization,
/// but not being finalized for that view (which happens if the consensus cannot finalize it
/// within the given view timeout period).
///
/// The type parameters `N` and `F` correspond to the total number of peers and the number of
/// faulty peers, respectively. The type parameter `M_SIZE` corresponds to the size of the
/// aggregated signature.
#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct MNotarization<const N: usize, const F: usize, const M_SIZE: usize> {
    /// The block that has been notarized
    pub block: Block,
    /// The aggregated signature of the block by the peers that have notarized it
    #[rkyv(with = ArkSerdeWrapper)]
    pub aggregated_signature: AggregatedSignature<M_SIZE>,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> MNotarization<N, F, M_SIZE> {
    pub fn new(block: Block, aggregated_signature: AggregatedSignature<M_SIZE>) -> Self {
        Self {
            block,
            aggregated_signature,
        }
    }

    /// Verifies the underlying M-notarization aggregated block signature
    pub fn verify(&self) -> bool {
        self.aggregated_signature.verify(&self.block.get_hash())
    }
}

/// An [`LNotarization`] corresponds to a majority vote of (n-f)-signatures
/// for a given view block. An L-notarization, once broadcast by a peer,
/// ensures that a given block has been fully finalized for a given view
/// `v <= current_view`.
#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct LNotarization<const N: usize, const F: usize, const L_SIZE: usize> {
    /// The block that has been notarized
    pub block: Block,
    /// The aggregated signature of the block by the peers that have voted for it
    #[rkyv(with = ArkSerdeWrapper)]
    pub aggregated_signature: AggregatedSignature<L_SIZE>,
}

impl<const N: usize, const F: usize, const L_SIZE: usize> LNotarization<N, F, L_SIZE> {
    pub fn new(block: Block, aggregated_signature: AggregatedSignature<L_SIZE>) -> Self {
        Self {
            block,
            aggregated_signature,
        }
    }

    /// Verifies the underlying M-notarization aggregated block signature
    pub fn verify(&self) -> bool {
        self.aggregated_signature.verify(&self.block.get_hash())
    }
}
