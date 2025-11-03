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
#[derive(Clone, Debug, Archive, Deserialize, Serialize)]
pub enum ConsensusMessage<const N: usize, const F: usize, const M_SIZE: usize> {
    BlockProposal(Block),
    Vote(Vote),
    Nullify(Nullify),
    MNotarization(MNotarization<N, F, M_SIZE>),
    Nullification(Nullification<N, F, M_SIZE>),
}
