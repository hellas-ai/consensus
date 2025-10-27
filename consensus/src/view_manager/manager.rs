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
        utils::{
            NotarizationData, NullificationData, create_notarization_data,
            create_nullification_data,
        },
        view_context::ViewContext,
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
}

impl<const N: usize, const F: usize, const M_SIZE: usize> ViewProgressManager<N, F, M_SIZE> {
    pub fn new(
        config: ConsensusConfig,
        replica_id: PeerId,
        leader_manager: Box<dyn LeaderManager>,
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
        })
    }

    /// Creates a new view progress manager from the genesis state. This is used
    /// to initialize the view progress manager when the consensus protocol starts.
    pub fn from_genesis(config: ConsensusConfig, replica_id: PeerId) -> Result<Self> {
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

        let block_hash = self.current_view_context.add_new_view_block(block)?;

        Ok(ViewProgressEvent::ShouldVote {
            view: self.current_view_context.view_number,
            block_hash,
        })
    }

    fn handle_new_vote(&mut self, vote: Vote) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        let peer_public_key = self
            .peers
            .get_public_key(&vote.peer_id)
            .expect("Peer not found");

        if !vote.verify(peer_public_key) {
            return Err(anyhow::anyhow!(
                "Vote signature is not valid for peer {}",
                vote.peer_id,
            ));
        }

        if vote.view > self.current_view {
            // TODO: Handle the case where a vote for a future view is received.
            // In this case, the replica should try to either sync up for the future view, or
            // ignore it altogether. Ideally, the replica would try to sync up for the future view,
            // but that involves more work, as it would require a supra-majority of replicas providing
            // more blocks to the current replica. We will implement this in a future PR. For now,
            // we will simply error out.
            return Err(anyhow::anyhow!(
                "Vote for view {} is for a future view (current view is {}), ignoring it for now.",
                vote.view,
                self.current_view,
            ));
        }

        if vote.view < self.current_view - 1 {
            return Err(anyhow::anyhow!(
                "Vote for view {} is for a previous view (current view is {}), ignoring it for now.",
                vote.view,
                self.current_view,
            ));
        }

        if !self.peers.contains(&vote.peer_id) {
            return Err(anyhow::anyhow!(
                "Vote for peer {} is not present in the peers set",
                vote.peer_id
            ));
        }

        // Check if the vote is already present in the votes set (either verified or non-verified)
        let has_already_voted_in_view = self.votes.iter().any(|v| v.peer_id == vote.peer_id)
            || self
                .non_verified_votes
                .iter()
                .any(|v| v.peer_id == vote.peer_id);

        if has_already_voted_in_view {
            return Err(anyhow::anyhow!(
                "Peer {} has already voted in view {}",
                vote.peer_id,
                self.current_view
            ));
        }

        // TODO: Refactor the order of the conditions in this block, as it might always be preferable
        if let Some(view_proposed_block_hash) = self.view_proposed_block_hash {
            let block_hash = vote.block_hash;
            if block_hash != view_proposed_block_hash {
                return Err(anyhow::anyhow!(
                    "Vote for block hash {} is not the view proposed block hash: {}",
                    hex::encode(block_hash),
                    hex::encode(view_proposed_block_hash)
                ));
            }
            self.votes.insert(vote);
            if self.votes.len() > 2 * F && self.m_notarizations.is_empty() {
                let NotarizationData {
                    peer_ids,
                    aggregated_signature,
                } = create_notarization_data::<M_SIZE>(&self.votes)?;
                // NOTE: The view for the [`MNotarization`] is the current view,
                // since the replica has not yet processed a M-notarization for the current view.
                // Therefore, it has not yet transitioned to the next view.
                self.m_notarizations.insert(MNotarization::new(
                    self.current_view,
                    block_hash,
                    aggregated_signature,
                    peer_ids,
                ));
                Ok(ViewProgressEvent::ShouldMNotarize {
                    view: self.current_view,
                    block_hash,
                })
            } else if self.m_notarizations.is_empty() {
                // TODO:
                todo!(
                    "Handle the case where votes + the current set of m-notarizations is enough to propose a new l-notarization"
                )
            } else if self.votes.len() > N - F {
                Ok(ViewProgressEvent::ShouldFinalize {
                    view: self.current_view,
                    block_hash,
                })
            } else {
                Ok(ViewProgressEvent::NoOp)
            }
        } else {
            // In this case, the replica has not yet received the view proposed block hash
            // from the leader, so we need to store the vote in the non-verified votes set.
            self.non_verified_votes.insert(vote);
            Ok(ViewProgressEvent::Await)
        }
    }

    fn handle_nullify(&mut self, nullify: Nullify) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        if nullify.view != self.current_view {
            return Err(anyhow::anyhow!(
                "Nullify for view {} is not the current view: {}",
                nullify.view,
                self.current_view
            ));
        }

        if !self.peers.contains(&nullify.peer_id) {
            return Err(anyhow::anyhow!(
                "Nullify for peer {} is not present in the peers set",
                nullify.peer_id
            ));
        }

        if !nullify.verify(
            self.peers
                .get_public_key(&nullify.peer_id)
                .expect("Peer not found"),
        ) {
            return Err(anyhow::anyhow!(
                "Nullify signature is not valid for peer {}",
                nullify.peer_id
            ));
        }

        if self
            .nullify_messages
            .iter()
            .any(|n| n.peer_id == nullify.peer_id)
        {
            return Err(anyhow::anyhow!(
                "Nullify for peer {} is already present in the nullify messages set",
                nullify.peer_id
            ));
        }

        self.nullify_messages.insert(nullify);
        if self.nullify_messages.len() > 2 * F {
            let NullificationData {
                peer_ids,
                aggregated_signature,
            } = create_nullification_data::<M_SIZE>(&self.nullify_messages)?;
            return Ok(ViewProgressEvent::ShouldBroadcastNullification {
                nullification: Nullification::new(
                    self.current_view,
                    self.leader_manager
                        .leader_for_view(self.current_view)?
                        .peer_id(),
                    aggregated_signature,
                    peer_ids,
                ),
            });
        }

        Ok(ViewProgressEvent::NoOp)
    }

    fn handle_m_notarization(
        &mut self,
        m_notarization: MNotarization<N, F, M_SIZE>,
    ) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        if self.is_previous_view_finalized && m_notarization.view != self.current_view {
            // If the previous view has been finalized, any m-notarization for a later view should be rejected.
            return Err(anyhow::anyhow!(
                "M-notarization for view {} is not the current view: {}",
                m_notarization.view,
                self.current_view
            ));
        }

        if !self.is_previous_view_finalized && m_notarization.view < self.current_view - 1 {
            return Err(anyhow::anyhow!(
                "M-notarization for view {} is not the previous view: {}",
                m_notarization.view,
                self.current_view
            ));
        }

        if m_notarization.view > self.current_view {
            // TODO: Handle the case where a m-notarization for a future view is received.
            // In this case, the replica should try to either sync up for the future view, or
            // ignore it altogether. Ideally, the replica would try to sync up for the future view,
            // but that involves more work, as it would require a supra-majority of replicas providing
            // more blocks to the current replica. We will implement this in a future PR. For now,
            // we will simply error out.
            return Err(anyhow::anyhow!(
                "M-notarization for view {} is for a future view (current view is {}), ignoring it for now.",
                m_notarization.view,
                self.current_view,
            ));
        }

        if self.view_proposed_block_hash.is_none() {
            // Check if the m-notarization contains a vote by the current view leader.
            return Err(anyhow::anyhow!(
                "View proposed block hash not found for the current view",
            ));
        }

        let view_proposed_block_hash = self
            .view_proposed_block_hash
            .expect("View proposed block hash not found");

        if m_notarization.block_hash != view_proposed_block_hash {
            return Err(anyhow::anyhow!(
                "Received M-notarization for block hash {} has a different block hash than the view proposed block hash: {}",
                hex::encode(m_notarization.block_hash),
                hex::encode(view_proposed_block_hash)
            ));
        }

        // TODO: The verification of the M-notarization should be ported to the p2p layer.
        if !m_notarization.verify(&self.peers) {
            return Err(anyhow::anyhow!(
                "M-notarization signature is not valid for the current view",
            ));
        }

        if self.m_notarizations.is_empty() {
            return Ok(ViewProgressEvent::ShouldFinalize {
                view: self.current_view,
                block_hash: m_notarization.block_hash,
            });
        }

        if self
            .m_notarizations
            .iter()
            .any(|m| m.block_hash == m_notarization.block_hash)
        {
            tracing::info!(
                "M-notarization for block hash {} is already present in the m-notarizations set",
                hex::encode(m_notarization.block_hash)
            );

            return Ok(ViewProgressEvent::NoOp);
        }

        Ok(ViewProgressEvent::ViewChanged {
            new_view: self.current_view + 1,
            leader: self
                .leader_manager
                .leader_for_view(self.current_view + 1)?
                .peer_id(),
        })
    }

    fn handle_nullification(
        &mut self,
        nullification: Nullification<N, F, M_SIZE>,
    ) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        if nullification.view != self.current_view && nullification.view < self.current_view - 1 {
            return Err(anyhow::anyhow!(
                "Nullification for view {} is not valid for the either the current view or the previous view: {}",
                nullification.view,
                self.current_view
            ));
        }

        if self.previous_finalized_block_hash.unwrap() != nullification.block_hash {
            return Err(anyhow::anyhow!(
                "Nullification for block hash {} is not the previous finalized block hash: {}",
                hex::encode(nullification.block_hash),
                hex::encode(self.previous_finalized_block_hash.unwrap())
            ));
        }

        if nullification.view == self.current_view {
            if !self.has_nullified_in_view {
                return Ok(ViewProgressEvent::ShouldNullify {
                    view: nullification.view,
                });
            } else {
                return Err(anyhow::anyhow!(
                    "Nullification for view {} is already present in the nullifications set",
                    nullification.view
                ));
            }
        } else if nullification.view == self.current_view - 1 {
        }

        if !self.peers.contains(&nullification.leader_id) {
            return Err(anyhow::anyhow!(
                "Nullification for leader {} is not present in the peers set",
                nullification.leader_id
            ));
        }

        if !nullification.verify(&self.peers) {
            return Err(anyhow::anyhow!(
                "Nullification signature is not valid for the view {}",
                nullification.view
            ));
        }

        if self.has_nullified_in_view {}

        Ok(ViewProgressEvent::NoOp)
    }

    fn try_update_view(&mut self, view: u64) -> Result<()> {
        // TODO: Implement view update logic
        tracing::info!("Trying to update view to {}", view);
        Ok(())
    }
}
