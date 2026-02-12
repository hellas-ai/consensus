use rkyv::{Archive, Deserialize, Serialize};

use crate::state::{
    block::Block,
    notarizations::{MNotarization, Vote},
    nullify::{Nullification, Nullify},
};

/// [`ConsensusMessage`] represents a message in the consensus protocol.
///
/// It can either be:
/// - A proposal for a new block, for the current view (it must be always proposed by the leader of
///   the current view)
/// - A vote for a block, for the current view (it can be proposed by any replica)
/// - A M-notarization for a block, for the current view (it can be proposed by any replica)
/// - A L-notarization for a block, for the current view (it can be proposed by any replica)
/// - A nullification for a view, for the current view (it can be proposed by any replica)
/// - A block recovery request/response for fetching missing blocks from peers
#[derive(Clone, Debug, Archive, Deserialize, Serialize)]
pub enum ConsensusMessage<const N: usize, const F: usize, const M_SIZE: usize> {
    BlockProposal(Block),
    Vote(Vote),
    Nullify(Nullify),
    MNotarization(MNotarization<N, F, M_SIZE>),
    Nullification(Nullification<N, F, M_SIZE>),
    /// Request a missing block by view and expected hash. Sent when a replica has M-notarization
    /// for a view but never received the actual block proposal from the leader.
    BlockRecoveryRequest { view: u64, block_hash: [u8; 32] },
    /// Response containing the requested block. Sent by a peer that has the block in its
    /// non-finalized view chain or finalized storage.
    BlockRecoveryResponse { view: u64, block: Block },
}
