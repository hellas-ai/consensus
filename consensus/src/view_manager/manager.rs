use std::{collections::HashSet, time::Instant};

use crate::{
    Block,
    state::{
        notarizations::{LNotarization, MNotarization, Vote},
        nullify::Nullification,
        transaction::Transaction,
    },
    view_manager::{
        config::ConsensusConfig,
        leader_manager::{LeaderManager, LeaderSelectionStrategy, RoundRobinLeaderManager},
    },
};

/// [`ViewProgressManager`] is the main service for the view progress of the underlying Minimmit consensus protocol.
///
/// It is responsible for managing the view progress of the consensus protocol,
/// including the leader selection, the block proposal, the voting, the nullification,
/// the M-notarization, the L-notarization, and the nullification.
pub struct ViewProgressManager<
    const N: usize,
    const F: usize,
    const M_SIZE: usize,
    const L_SIZE: usize,
> {
    /// The configuration of the consensus protocol.
    config: ConsensusConfig,
    /// The leader manager algorithm to use for leader selection.
    leader_manager: Box<dyn LeaderManager>,

    /// The current view
    current_view: u64,
    /// The start time of the current view (measured in milliseconds by the current replica)
    view_start_time: Instant,
    /// The actual replica has already voted for the current view
    has_voted_in_view: bool,
    /// The block for which the actual replica has already voted for
    voted_block_hash: Option<[u8; blake3::OUT_LEN]>,
    /// If the current replica has proposed a block for the current view
    has_proposed_in_view: bool,
    /// If the current replica has proposed a nullified message for the current view
    has_nullified_in_view: bool,

    // In-memory state for current view
    /// The block received for the current view (if any).
    block: Option<Block>,
    /// Received votes for the current view's block
    votes: HashSet<Vote>,
    /// Nullify messages for the current view, we
    /// just need to store the peer id for each replica.
    nullify_messages: HashSet<u64>,
    /// Nullifications for the current view
    nullifications: HashSet<Nullification<N, F, M_SIZE>>,
    /// Current M-notarizations for the current view's block
    m_notarizations: HashSet<MNotarization<N, F, M_SIZE>>,
    /// Current L-notarizations for the current view's block
    l_notarizations: HashSet<LNotarization<N, F, L_SIZE>>,

    /// Transaction pool
    pending_txs: Vec<Transaction>,
}

impl<const N: usize, const F: usize, const M_SIZE: usize, const L_SIZE: usize>
    ViewProgressManager<N, F, M_SIZE, L_SIZE>
{
    pub fn new(config: ConsensusConfig, leader_manager: Box<dyn LeaderManager>) -> Self {
        Self {
            config,
            leader_manager,
            current_view: 1,
            view_start_time: Instant::now(),
            has_voted_in_view: false,
            voted_block_hash: None,
            has_proposed_in_view: false,
            has_nullified_in_view: false,
            block: None,
            votes: HashSet::new(),
            nullify_messages: HashSet::new(),
            nullifications: HashSet::new(),
            m_notarizations: HashSet::new(),
            l_notarizations: HashSet::new(),
            pending_txs: Vec::new(),
        }
    }

    /// Creates a new view progress manager from the genesis state. This is used
    /// to initialize the view progress manager when the consensus protocol starts.
    pub fn from_genesis(config: ConsensusConfig) -> Self {
        let leader_manager = match config.leader_manager {
            LeaderSelectionStrategy::RoundRobin => {
                Box::new(RoundRobinLeaderManager::new(config.n, Vec::new()))
            }
            LeaderSelectionStrategy::Random => Box::new(todo!()),
            LeaderSelectionStrategy::ProofOfStake => Box::new(todo!()),
        };
        Self {
            config,
            leader_manager,
            current_view: 0,
            view_start_time: Instant::now(),
            has_voted_in_view: false,
            voted_block_hash: None,
            has_proposed_in_view: false,
            has_nullified_in_view: false,
            block: None,
            votes: HashSet::new(),
            nullify_messages: HashSet::new(),
            nullifications: HashSet::new(),
            m_notarizations: HashSet::new(),
            l_notarizations: HashSet::new(),
            pending_txs: Vec::new(),
        }
    }
}
