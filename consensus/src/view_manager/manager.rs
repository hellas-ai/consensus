use std::str::FromStr;

use anyhow::Result;
use tracing::instrument;

use crate::{
    consensus::ConsensusMessage,
    crypto::aggregated::{BlsPublicKey, PeerId},
    state::{
        block::Block,
        notarizations::{MNotarization, Vote},
        nullify::{Nullification, Nullify},
        peer::PeerSet,
        transaction::Transaction,
    },
    view_manager::{
        config::ConsensusConfig,
        events::ViewProgressEvent,
        leader_manager::{LeaderManager, LeaderSelectionStrategy, RoundRobinLeaderManager},
        view_context::{
            CollectedNullificationsResult, CollectedVotesResult, LeaderProposalResult,
            ShouldMNotarize, ViewContext,
        },
    },
};

/// [`ViewProgressManager`] is the main service for the view progress of the underlying Minimmit consensus protocol.
///
/// It is responsible for managing the view progress of the consensus protocol,
/// including the leader selection, the block proposal, the voting, the nullification,
/// the M-notarization, the L-notarization, and the nullification.
pub struct ViewProgressManager<const N: usize, const F: usize, const M_SIZE: usize> {
    /// The configuration of the consensus protocol.
    config: ConsensusConfig,

    /// The leader manager algorithm to use for leader selection.
    #[allow(unused)]
    leader_manager: Box<dyn LeaderManager>,

    /// The per-view context tracking
    current_view_context: ViewContext<N, F, M_SIZE>,

    /// The un-finalized view context (if any)
    ///
    /// This is a view not yet finalized by a supra-majority vote (n-f) or a nullification.
    /// But that has already received a m-notarization, and therefore, the replica has
    /// progressed to the next view.
    unfinalized_view_context: Option<ViewContext<N, F, M_SIZE>>,

    /// The set of peers in the consensus protocol.
    #[allow(unused)]
    peers: PeerSet,

    /// Transaction pool
    pending_txs: Vec<Transaction>,

    /// The logger
    _logger: slog::Logger,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> ViewProgressManager<N, F, M_SIZE> {
    pub fn new(
        config: ConsensusConfig,
        replica_id: PeerId,
        leader_manager: Box<dyn LeaderManager>,
        logger: slog::Logger,
    ) -> Result<Self> {
        let leader_id = leader_manager.leader_for_view(0)?.peer_id();
        let peers = PeerSet::new(
            config
                .peers
                .iter()
                .map(|p| BlsPublicKey::from_str(p).expect("Failed to parse BlsPublicKey"))
                .collect(),
        );
        let view_context = ViewContext::new(0, leader_id, replica_id, [0; blake3::OUT_LEN]);
        Ok(Self {
            config,
            leader_manager,
            current_view_context: view_context,
            unfinalized_view_context: None,
            peers,
            pending_txs: Vec::new(),
            _logger: logger,
        })
    }

    /// Creates a new view progress manager from the genesis state. This is used
    /// to initialize the view progress manager when the consensus protocol starts.
    pub fn from_genesis(
        config: ConsensusConfig,
        replica_id: PeerId,
        logger: slog::Logger,
    ) -> Result<Self> {
        let leader_manager = match config.leader_manager {
            LeaderSelectionStrategy::RoundRobin => {
                Box::new(RoundRobinLeaderManager::new(config.n, Vec::new()))
            }
            #[allow(unreachable_code)]
            LeaderSelectionStrategy::Random => Box::new(todo!()),
            #[allow(unreachable_code)]
            LeaderSelectionStrategy::ProofOfStake => Box::new(todo!()),
        };
        let peers = PeerSet::new(
            config
                .peers
                .iter()
                .map(|p| BlsPublicKey::from_str(p).expect("Failed to parse BlsPublicKey"))
                .collect(),
        );
        let leader_id = leader_manager.leader_for_view(0)?.peer_id();
        let view_context = ViewContext::new(0, leader_id, replica_id, [0; blake3::OUT_LEN]);
        Ok(Self {
            config,
            leader_manager,
            current_view_context: view_context,
            unfinalized_view_context: None,
            peers,
            pending_txs: Vec::new(),
            _logger: logger,
        })
    }

    /// [`process_consensus_msg`] is the main driver of the underlying state machine
    /// replication algorithm. Based on received [`ConsensusMessage`], it processes
    /// these and makes sure progress the SMR whenever possible.
    pub fn process_consensus_msg(
        &mut self,
        consensus_message: ConsensusMessage<N, F, M_SIZE>,
    ) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        match consensus_message {
            ConsensusMessage::BlockProposal(block) => self.handle_block_proposal(block),
            ConsensusMessage::Vote(vote) => self.handle_new_vote(vote),
            ConsensusMessage::Nullify(nullify) => self.handle_nullify(nullify),
            ConsensusMessage::MNotarization(m_notarization) => {
                self.handle_m_notarization(m_notarization)
            }
            ConsensusMessage::Nullification(nullification) => {
                self.handle_nullification(nullification)
            }
        }
    }

    /// Called periodically to check timers and trigger view changes if needed
    #[instrument("debug", skip_all)]
    pub fn tick(&mut self) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        if self.current_view_context.is_leader() && !self.current_view_context.has_proposed {
            if let Some(parent_block_hash) = self.select_parent() {
                return Ok(ViewProgressEvent::ShouldProposeBlock {
                    view: self.current_view_context.view_number,
                    parent_block_hash,
                });
            } else {
                return Err(anyhow::anyhow!(
                    "Failed to retrieve a parent block hash for the current view: {}",
                    self.current_view_context.view_number
                ));
            }
        }

        if !self.current_view_context.has_nullified
            && self.current_view_context.entered_at.elapsed() >= self.config.view_timeout
        {
            self.current_view_context.has_nullified = true;
            return Ok(ViewProgressEvent::ShouldNullify {
                view: self.current_view_context.view_number,
            });
        }

        if !self.current_view_context.has_voted && !self.current_view_context.has_nullified {
            if let Some(block) = &self.current_view_context.block {
                self.current_view_context.has_voted = true;
                self.current_view_context.block_hash = Some(block.get_hash());
                return Ok(ViewProgressEvent::ShouldVote {
                    view: self.current_view_context.view_number,
                    block_hash: block.get_hash(),
                });
            }
            Ok(ViewProgressEvent::Await)
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

    /// [`handle_block_proposal`] is called when the current replica receives a new block proposal,
    /// from the leader of the current view.
    ///
    /// It validates the block and adds it to the current view context.
    /// If the block is for a future view, it returns a [`ViewProgressEvent::ShouldUpdateView`]
    /// event, to update the view context to the future view.
    /// If the block is for the current view, and it passes all validation checks in
    /// [`ViewContext::add_new_view_block`], then the method returns a [`ViewProgressEvent::ShouldVote`]
    /// event, to vote for the block.
    fn handle_block_proposal(&mut self, block: Block) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        // Validate block for the current view
        if block.header.view > self.current_view_context.view_number {
            // If the block is for a future view, then we need to update the view context to the future view,
            // if the leader of the future view is not the current leader.
            let block_view_leader = self
                .leader_manager
                .leader_for_view(block.header.view)?
                .peer_id();
            if block_view_leader != block.leader {
                return Err(anyhow::anyhow!(
                    "Block leader {} is not the correct view leader {} for block's view {}",
                    block_view_leader,
                    block.leader,
                    block.view()
                ));
            }
            return Ok(ViewProgressEvent::ShouldUpdateView {
                new_view: block.header.view,
                leader: block_view_leader,
            });
        }

        let LeaderProposalResult {
            block_hash,
            is_enough_to_m_notarize,
            is_enough_to_finalize,
        } = self.current_view_context.add_new_view_block(block)?;

        if is_enough_to_m_notarize {
            // NOTE: In this case, the replica has collected enough votes to propose a M-notarization,
            // but not enough to finalize the view. Therefore, the replica should vote for the block
            // and notarize it simultaneously.
            return Ok(ViewProgressEvent::ShouldVoteAndMNotarize {
                view: self.current_view_context.view_number,
                block_hash,
            });
        } else if is_enough_to_finalize {
            // NOTE: In this case, the replica has collected enough votes to finalize the view,
            // before it has received the block proposal from the leader, as most likely the replica
            // was beyond. In such case, the replica should vote for the block and finalize the view simultaneously.
            return Ok(ViewProgressEvent::ShouldVoteAndFinalize {
                view: self.current_view_context.view_number,
                block_hash,
            });
        }

        Ok(ViewProgressEvent::ShouldVote {
            view: self.current_view_context.view_number,
            block_hash,
        })
    }

    fn handle_new_vote(&mut self, vote: Vote) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        if vote.view > self.current_view_context.view_number {
            // TODO: Handle the case where a vote for a future view is received.
            // In this case, the replica should try to either sync up for the future view, or
            // ignore it altogether. Ideally, the replica would try to sync up for the future view,
            // but that involves more work, as it would require a supra-majority of replicas providing
            // more blocks to the current replica.
            let block_view_leader = self.leader_manager.leader_for_view(vote.view)?.peer_id();
            if block_view_leader != vote.leader_id {
                return Err(anyhow::anyhow!(
                    "Vote for leader {} is not the correct view leader {} for vote's view {}",
                    vote.leader_id,
                    block_view_leader,
                    vote.view
                ));
            }
            return Ok(ViewProgressEvent::ShouldUpdateView {
                new_view: vote.view,
                leader: block_view_leader,
            });
        }

        if self.current_view_context.view_number == vote.view {
            let CollectedVotesResult {
                should_await,
                is_enough_to_m_notarize,
                is_enough_to_finalize,
            } = self.current_view_context.add_vote(vote, &self.peers)?;
            if should_await {
                return Ok(ViewProgressEvent::Await);
            } else if is_enough_to_m_notarize {
                return Ok(ViewProgressEvent::ShouldMNotarize {
                    view: self.current_view_context.view_number,
                    block_hash: self.current_view_context.block_hash.unwrap(),
                });
            } else if is_enough_to_finalize {
                return Ok(ViewProgressEvent::ShouldFinalize {
                    view: self.current_view_context.view_number,
                    block_hash: self.current_view_context.block_hash.unwrap(),
                });
            } else {
                return Ok(ViewProgressEvent::NoOp);
            }
        }

        if let Some(ref mut unfinalized_view_context) = self.unfinalized_view_context {
            unfinalized_view_context.has_view_progressed_without_m_notarization()?;
            if vote.view == unfinalized_view_context.view_number {
                let CollectedVotesResult {
                    should_await,
                    is_enough_to_m_notarize,
                    is_enough_to_finalize,
                } = unfinalized_view_context.add_vote(vote, &self.peers)?;
                if should_await {
                    return Ok(ViewProgressEvent::Await);
                } else if is_enough_to_m_notarize {
                    return Ok(ViewProgressEvent::ShouldMNotarize {
                        view: unfinalized_view_context.view_number,
                        block_hash: unfinalized_view_context.block_hash.unwrap(),
                    });
                } else if is_enough_to_finalize {
                    return Ok(ViewProgressEvent::ShouldFinalize {
                        view: unfinalized_view_context.view_number,
                        block_hash: unfinalized_view_context.block_hash.unwrap(),
                    });
                } else {
                    return Ok(ViewProgressEvent::NoOp);
                }
            }
        }

        Err(anyhow::anyhow!(
            "Vote for view {} is not the current view {} or the unfinalized view {}",
            vote.view,
            self.current_view_context.view_number,
            self.current_view_context.view_number - 1,
        ))
    }

    fn handle_nullify(&mut self, nullify: Nullify) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        if nullify.view > self.current_view_context.view_number {
            let block_view_leader = self.leader_manager.leader_for_view(nullify.view)?.peer_id();
            if block_view_leader != nullify.leader_id {
                return Err(anyhow::anyhow!(
                    "Nullify for leader {} is not the correct view leader {} for nullify's view {}",
                    nullify.leader_id,
                    block_view_leader,
                    nullify.view
                ));
            }
            return Ok(ViewProgressEvent::ShouldUpdateView {
                new_view: nullify.view,
                leader: block_view_leader,
            });
        }

        if self.current_view_context.view_number == nullify.view {
            self.current_view_context
                .add_nullify(nullify, &self.peers)?;
            if self.current_view_context.nullification.is_some() {
                return Ok(ViewProgressEvent::ShouldBroadcastNullification {
                    view: self.current_view_context.view_number,
                });
            }
            return Ok(ViewProgressEvent::NoOp);
        }

        if let Some(ref mut unfinalized_view_context) = self.unfinalized_view_context {
            unfinalized_view_context.has_view_progressed_without_m_notarization()?;
            if nullify.view == unfinalized_view_context.view_number {
                unfinalized_view_context.add_nullify(nullify, &self.peers)?;
                if unfinalized_view_context.nullification.is_some() {
                    return Ok(ViewProgressEvent::ShouldBroadcastNullification {
                        view: unfinalized_view_context.view_number,
                    });
                }
                return Ok(ViewProgressEvent::NoOp);
            }
        }

        Err(anyhow::anyhow!(
            "Nullify for view {} is not the current view {} or the unfinalized view {}",
            nullify.view,
            self.current_view_context.view_number,
            self.current_view_context.view_number - 1,
        ))
    }

    fn handle_m_notarization(
        &mut self,
        m_notarization: MNotarization<N, F, M_SIZE>,
    ) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        if self.current_view_context.view_number < m_notarization.view {
            let block_view_leader = self
                .leader_manager
                .leader_for_view(m_notarization.view)?
                .peer_id();
            if block_view_leader != m_notarization.leader_id {
                return Err(anyhow::anyhow!(
                    "M-notarization for leader {} is not the correct view leader {} for m-notarization's view {}",
                    m_notarization.leader_id,
                    block_view_leader,
                    m_notarization.view
                ));
            }
            return Ok(ViewProgressEvent::ShouldUpdateView {
                new_view: m_notarization.view,
                leader: block_view_leader,
            });
        }

        if self.current_view_context.view_number == m_notarization.view {
            let ShouldMNotarize {
                should_notarize,
                should_await,
            } = self
                .current_view_context
                .add_m_notarization(m_notarization, &self.peers)?;
            if should_notarize {
                return Ok(ViewProgressEvent::ProgressToNextView {
                    new_view: self.current_view_context.view_number + 1,
                    leader: self
                        .leader_manager
                        .leader_for_view(self.current_view_context.view_number + 1)?
                        .peer_id(),
                });
            }
            if should_await {
                return Ok(ViewProgressEvent::Await);
            }
            return Ok(ViewProgressEvent::NoOp);
        }

        if let Some(ref unfinalized_view_context) = self.unfinalized_view_context {
            unfinalized_view_context.has_view_progressed_without_m_notarization()?;
            if m_notarization.view == unfinalized_view_context.view_number {
                // NOTE: There is not anything left to do here, as the m-notarization has already been added to the unfinalized view context.
                return Ok(ViewProgressEvent::NoOp);
            }
        }

        Err(anyhow::anyhow!(
            "M-notarization for view {} is not the current view {} or the unfinalized view {}",
            m_notarization.view,
            self.current_view_context.view_number,
            self.current_view_context.view_number - 1,
        ))
    }

    fn handle_nullification(
        &mut self,
        nullification: Nullification<N, F, M_SIZE>,
    ) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        if nullification.view > self.current_view_context.view_number {
            let block_view_leader = self
                .leader_manager
                .leader_for_view(nullification.view)?
                .peer_id();
            if block_view_leader != nullification.leader_id {
                return Err(anyhow::anyhow!(
                    "Nullification for leader {} is not the correct view leader {} for nullification's view {}",
                    nullification.leader_id,
                    block_view_leader,
                    nullification.view
                ));
            }
            return Ok(ViewProgressEvent::ShouldUpdateView {
                new_view: nullification.view,
                leader: block_view_leader,
            });
        }

        if self.current_view_context.view_number == nullification.view {
            let CollectedNullificationsResult {
                should_broadcast_nullification,
            } = self
                .current_view_context
                .add_nullification(nullification, &self.peers)?;
            if should_broadcast_nullification {
                return Ok(ViewProgressEvent::ShouldBroadcastNullification {
                    view: self.current_view_context.view_number,
                });
            }
            return Ok(ViewProgressEvent::ShouldNullify {
                view: self.current_view_context.view_number,
            });
        }

        if let Some(ref mut unfinalized_view_context) = self.unfinalized_view_context {
            unfinalized_view_context.has_view_progressed_without_m_notarization()?;
            if nullification.view == unfinalized_view_context.view_number {
                let CollectedNullificationsResult {
                    should_broadcast_nullification,
                } = unfinalized_view_context.add_nullification(nullification, &self.peers)?;
                if should_broadcast_nullification {
                    return Ok(ViewProgressEvent::ShouldBroadcastNullification {
                        view: unfinalized_view_context.view_number,
                    });
                } else {
                    return Ok(ViewProgressEvent::ShouldNullify {
                        view: unfinalized_view_context.view_number,
                    });
                }
            }
        }

        Err(anyhow::anyhow!(
            "Nullification for view {} is not the current view {} or the unfinalized view {}",
            nullification.view,
            self.current_view_context.view_number,
            self.current_view_context.view_number - 1,
        ))
    }

    fn _try_update_view(&mut self, view: u64) -> Result<()> {
        // TODO: Implement view update logic
        tracing::info!("Trying to update view to {}", view);
        Ok(())
    }
}
