use crate::{consensus::ConsensusMessage, crypto::aggregated::PeerId};

/// [`ViewProgressEvent`] represents an event that occurs in the view progress manager.
///
/// It can either be:
/// - A request to propose a block for the current view
/// - A request to vote for a block for the current view
/// - A request to nullify the current view
/// - A request to notarize a block for the current view
/// - A request to finalize a block for the current view
/// - A request to change the view
/// - A request to broadcast a consensus message
///
/// The type parameters `N`, `F`, `M_SIZE`, and `L_SIZE` correspond to the total number of replicas,
/// the number of faulty replicas, the size of the aggregated signature for M-notarizations,
/// and the size of the aggregated signature for L-notarizations, respectively.
#[derive(Clone, Debug)]
pub enum ViewProgressEvent<const N: usize, const F: usize, const M_SIZE: usize> {
    /// If the current replica is the leader for the current `view`,
    /// and it should propose a block for the current view.
    ShouldProposeBlock {
        /// Current view number (for which the replica is the leader).
        view: u64,
        /// The hash of the parent block (that is, the last finalized block
        /// in the protocol).
        parent_block_hash: [u8; blake3::OUT_LEN],
    },

    /// If the current replica should vote for a block for the current view.
    ShouldVote {
        /// Current view number (for which the replica should vote).
        view: u64,
        /// The hash of the block that the replica should vote for.
        block_hash: [u8; blake3::OUT_LEN],
    },

    /// If the current replica should nullify the current view.
    ShouldNullify {
        /// Current view number (for which the replica should nullify).
        view: u64,
    },

    /// If the current replica should broadcast a nullification for the current view.
    ShouldBroadcastNullification {
        /// The view number for which the replica should nullify.
        view: u64,
    },

    /// If the current replica should M-notarize a block for the current view.
    ShouldMNotarize {
        /// Current view number (for which the replica should notarize).
        view: u64,
        /// The hash of the block that the replica should notarize.
        block_hash: [u8; blake3::OUT_LEN],
    },

    /// If the current replica should notarize a block for the current view, and vote for it
    /// simultaneously.
    ShouldVoteAndMNotarize {
        /// Current view number (for which the replica should notarize and vote).
        view: u64,
        /// The hash of the block that the replica should notarize and vote for.
        block_hash: [u8; blake3::OUT_LEN],
    },

    /// If the current replica should finalize the state for the the `view`.
    /// Notice that, `view` does not necessarily correspond to the current view,
    /// as the replica might have already progressed to a later view (in case,
    /// it has received a M-notarization for `view`, or a nullification).
    ShouldFinalize {
        /// Current view number (for which the replica should finalize).
        view: u64,
        /// The hash of the block that the replica should finalize.
        /// In case it is `None`, the replica should nullify the `view`,
        /// that is, the state machine replication protocol hasn't made
        /// any actual progress (due to a failure to collect enough votes
        /// to finalize a block for the `view`, and/or leader failure).
        block_hash: [u8; blake3::OUT_LEN],
    },

    /// If the current replica should vote for a block for the current view, and finalize it
    /// simultaneously.
    ShouldVoteAndFinalize {
        /// Current view number (for which the replica should vote and finalize).
        view: u64,
        /// The hash of the block that the replica should vote and finalize for.
        block_hash: [u8; blake3::OUT_LEN],
    },

    /// If the current replica should progress to a new view. This happens
    /// whenever the current replica receives either a M-notarization or a nullification
    /// for the current view.
    ProgressToNextView {
        /// New view number (for which the replica should change).
        new_view: u64,
        /// The leader's ID of the new view.
        leader: PeerId,
    },

    /// If the current replica should broadcast a consensus message
    BroadcastConsensusMessage {
        /// The consensus message to be broadcasted by the current replica
        /// to its peers on the network.
        message: Box<ConsensusMessage<N, F, M_SIZE>>,
    },

    /// No operation is required at the moment, since (most likely) the replica
    /// already has made its state progress for the current view.
    NoOp,

    /// The replica still has not been able to make progress on the current view,
    /// and the view timeout has not been triggered yet, so the replica must await
    /// until it either receives a block to vote on the current view, or the timeout
    /// is triggered.
    Await,

    /// If the current replica should update the view context to a new view (in the future).
    ///
    /// This happens whenever the current replica receives a block for a future view,
    /// and the leader of the future view is not the current leader.
    ShouldUpdateView {
        /// New view number (for which the replica should change).
        new_view: u64,

        /// The leader's ID of the new view.
        leader: PeerId,
    },

    /// If the current replica should nullify the current view.
    ShouldNullifyView {
        /// View number (for which the replica should nullify).
        view: u64,
    },
}
