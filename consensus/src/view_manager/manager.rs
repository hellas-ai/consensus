use std::{collections::HashSet, time::Instant};

use anyhow::Result;
use tracing::instrument;

use crate::{
    consensus::ConsensusMessage,
    state::{
        block::Block,
        notarizations::{LNotarization, MNotarization, Vote},
        nullify::Nullification,
        transaction::Transaction,
    },
    view_manager::{
        config::ConsensusConfig,
        events::ViewProgressEvent,
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
    #[allow(unused)]
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
    /// If the current replica is the leader for the current view
    is_leader: bool,

    // In-memory state for current view
    /// The block received for the current view (if any).
    block: Option<Block>,
    /// Received votes for the current view's block
    #[allow(unused)]
    votes: HashSet<Vote>,
    /// Nullify messages for the current view, we
    /// just need to store the peer id for each replica.
    #[allow(unused)]
    nullify_messages: HashSet<u64>,
    /// Nullifications for the current view
    #[allow(unused)]
    nullifications: HashSet<Nullification<N, F, M_SIZE>>,
    /// Current M-notarizations for the current view's block
    #[allow(unused)]
    m_notarizations: HashSet<MNotarization<N, F, M_SIZE>>,
    /// Current L-notarizations for the current view's block
    #[allow(unused)]
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
            is_leader: todo!(),
            #[allow(unreachable_code)]
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
            #[allow(unreachable_code)]
            LeaderSelectionStrategy::Random => Box::new(todo!()),
            #[allow(unreachable_code)]
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
            is_leader: todo!(),
            #[allow(unreachable_code)]
            block: None,
            votes: HashSet::new(),
            nullify_messages: HashSet::new(),
            nullifications: HashSet::new(),
            m_notarizations: HashSet::new(),
            l_notarizations: HashSet::new(),
            pending_txs: Vec::new(),
        }
    }

    /// [`process_consensus_msg`] is the main driver of the underlying state machine
    /// replication algorithm. Based on received [`ConsensusMessage`], it processes
    /// these and makes sure progress the SMR whenever possible.
    pub fn process_consensus_msg(
        &self,
        consensus_message: ConsensusMessage<N, F, M_SIZE, L_SIZE>,
    ) -> Result<ViewProgressEvent<N, F, M_SIZE, L_SIZE>> {
        match consensus_message {
            ConsensusMessage::BlockProposal(_block) => {
                todo!()
            }
            ConsensusMessage::Vote(_vote) => {
                todo!()
            }
            ConsensusMessage::Nullify(_view) => {
                todo!()
            }
            ConsensusMessage::MNotarization(_m_notarization) => {
                todo!()
            }
            ConsensusMessage::LNotarization(_l_notarization) => {
                todo!()
            }
            ConsensusMessage::Nullification(_nullification) => {
                todo!()
            }
        }
    }

    /// Called periodically to check timers and trigger view changes if needed
    #[instrument("debug", skip_all)]
    pub fn tick(&mut self) -> Result<ViewProgressEvent<N, F, M_SIZE, L_SIZE>> {
        if self.is_leader && !self.has_proposed_in_view {
            if let Some(parent_block_hash) = self.select_parent() {
                return Ok(ViewProgressEvent::ShouldProposeBlock {
                    view: self.current_view,
                    parent_block_hash,
                });
            } else {
                return Err(anyhow::anyhow!(
                    "Failed to retrieve a parent block hash for the current view: {}",
                    self.current_view
                ));
            }
        }

        if !self.has_nullified_in_view && self.view_start_time.elapsed() >= self.config.view_timeout
        {
            self.has_nullified_in_view = true;
            return Ok(ViewProgressEvent::ShouldNullify {
                view: self.current_view,
            });
        }

        if !self.has_voted_in_view && !self.has_nullified_in_view {
            if let Some(block) = &self.block {
                self.has_voted_in_view = true;
                self.voted_block_hash = Some(block.get_hash());
                Ok(ViewProgressEvent::ShouldVote {
                    view: self.current_view,
                    block_hash: block.get_hash(),
                })
            } else {
                // In this case, the replica still hasn't received a [`Block`]
                // for the current view, and it hasn't nullify the view (possibly
                // because the timeout hasn't been triggered yet). In this case,
                // we return a NoOp
                Ok(ViewProgressEvent::Await)
            }
        } else {
            // In this case, the replica has either voted for a block, or nullified the current view.
            // In both cases, we can return a no-op event.
            Ok(ViewProgressEvent::NoOp)
        }
    }

    /// Adds a new transaction to the replica's `pending_transactions` values.
    pub fn add_transaction(&mut self, tx: Transaction) {
        self.pending_txs.push(tx)
    }

    pub fn select_parent(&self) -> Option<[u8; blake3::OUT_LEN]> {
        todo!()
    }
}
