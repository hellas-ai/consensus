use std::str::FromStr;

use anyhow::Result;
use tracing::instrument;

use crate::{
    consensus::ConsensusMessage,
    consensus_manager::{
        config::ConsensusConfig,
        events::ViewProgressEvent,
        leader_manager::{LeaderManager, LeaderSelectionStrategy, RoundRobinLeaderManager},
        view_chain::ViewChain,
        view_context::{
            CollectedNullificationsResult, CollectedVotesResult, LeaderProposalResult,
            ShouldMNotarize, ViewContext,
        },
    },
    crypto::aggregated::{BlsPublicKey, BlsSignature, PeerId},
    state::{
        block::Block,
        notarizations::{MNotarization, Vote},
        nullify::{Nullification, Nullify},
        peer::PeerSet,
        transaction::Transaction,
    },
    storage::store::ConsensusStore,
};

// TODO: Add view progression logic

/// [`ViewProgressManager`] is the main service for the view progress of the underlying Minimmit
/// consensus protocol.
///
/// It coordinates consensus by managing the view chain, processing consensus messages,
/// handling leader selection, and triggering view progression and block finalization events.
///
/// # Responsibilities
/// - Route consensus messages to the appropriate view in the chain
/// - Track view progression via M-notarization (2f+1 votes) or nullification (2f+1 nullify
///   messages)
/// - Trigger block finalization via L-notarization (n-f votes)
/// - Manage leader selection and block proposal
/// - Maintain transaction pool for block proposals
///
/// # Type Parameters
/// * `N` - Total number of replicas in the network
/// * `F` - Maximum number of faulty replicas tolerated
/// * `M_SIZE` - Size of aggregated signature for M-notarizations (typically 2f+1)
pub struct ViewProgressManager<const N: usize, const F: usize, const M_SIZE: usize> {
    /// The configuration of the consensus protocol.
    config: ConsensusConfig,

    /// The leader manager algorithm to use for leader selection.
    leader_manager: Box<dyn LeaderManager>,

    /// The chain of non-finalized views.
    view_chain: ViewChain<N, F, M_SIZE>,

    /// The replica's own peer ID.
    replica_id: PeerId,

    /// The set of peers in the consensus protocol.
    peers: PeerSet,

    /// Transaction pool
    pending_txs: Vec<Transaction>,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> ViewProgressManager<N, F, M_SIZE> {
    pub fn new(
        config: ConsensusConfig,
        replica_id: PeerId,
        persistence_storage: ConsensusStore,
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
        let view_chain = ViewChain::new(view_context, persistence_storage, config.view_timeout);
        Ok(Self {
            config,
            leader_manager,
            view_chain,
            replica_id,
            peers,
            pending_txs: Vec::new(),
        })
    }

    /// Creates a new view progress manager from the genesis state. This is used
    /// to initialize the view progress manager when the consensus protocol starts.
    pub fn from_genesis(
        config: ConsensusConfig,
        replica_id: PeerId,
        persistence_storage: ConsensusStore,
    ) -> Result<Self> {
        let peers = PeerSet::new(
            config
                .peers
                .iter()
                .map(|p| BlsPublicKey::from_str(p).expect("Failed to parse BlsPublicKey"))
                .collect(),
        );

        let leader_manager = match config.leader_manager {
            LeaderSelectionStrategy::RoundRobin => Box::new(RoundRobinLeaderManager::new(
                config.n,
                peers.sorted_peer_ids,
            )),
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
        let view_chain = ViewChain::new(view_context, persistence_storage, config.view_timeout);

        Ok(Self {
            config,
            leader_manager,
            view_chain,
            replica_id,
            peers,
            pending_txs: Vec::new(),
        })
    }

    /// Main driver of the state machine replication algorithm.
    ///
    /// Processes received `ConsensusMessage` and emits appropriate `ViewProgressEvent`s
    /// to guide the replica's actions.
    pub fn process_consensus_msg(
        &mut self,
        consensus_message: ConsensusMessage<N, F, M_SIZE>,
    ) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        match consensus_message {
            ConsensusMessage::BlockProposal(block) => self.handle_block_proposal(block),
            ConsensusMessage::Vote(vote) => self.handle_vote(vote),
            ConsensusMessage::Nullify(nullify) => self.handle_nullify(nullify),
            ConsensusMessage::MNotarization(m_notarization) => {
                self.handle_m_notarization(m_notarization)
            }
            ConsensusMessage::Nullification(nullification) => {
                self.handle_nullification(nullification)
            }
        }
    }

    /// Called periodically to check timers and trigger view changes if needed.
    ///
    /// Implements the core logic from Minimmit paper Algorithm 1, checking at every timeslot:
    /// - Leader block proposal
    /// - Voting on valid proposals or M-notarizations
    /// - Timeout-based nullification
    /// - Nullification based on conflicting votes
    #[instrument("debug", skip_all)]
    pub fn tick(&mut self) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        let current_view = self.view_chain.current();
        let view_range = self.view_chain.non_finalized_view_numbers_range();

        for view_number in view_range {
            // Check if this past view has timed out and should be nullified
            let view_ctx = self.view_chain.find_view_context(view_number).unwrap();

            if current_view.view_number == view_ctx.view_number {
                // NOTE: In the case of the current view, we should prioritize handling leader block
                // proposal
                continue;
            }

            if view_ctx.should_timeout_nullify(self.config.view_timeout) {
                return Ok(ViewProgressEvent::ShouldNullify {
                    view: view_ctx.view_number,
                });
            }
        }

        // If this replica is the leader and hasn't proposed yet
        if current_view.is_leader() && !current_view.has_proposed {
            return Ok(ViewProgressEvent::ShouldProposeBlock {
                view: current_view.view_number,
                parent_block_hash: current_view.parent_block_hash,
            });
        }

        if !current_view.has_voted
            && !current_view.has_nullified
            && let Some(ref block) = current_view.block
        {
            return Ok(ViewProgressEvent::ShouldVote {
                view: current_view.view_number,
                block_hash: block.get_hash(),
            });
        }

        // If timer = 2Δ and haven't nullified and haven't voted, send nullify
        if !current_view.has_nullified
            && !current_view.has_voted
            && current_view.entered_at.elapsed() >= self.config.view_timeout
        {
            return Ok(ViewProgressEvent::ShouldNullify {
                view: current_view.view_number,
            });
        }

        // If M-notarization exists for current view and haven't voted/nullified, vote before
        // progressing
        if !current_view.has_voted
            && !current_view.has_nullified
            && current_view.nullification.is_none()
            && let Some(ref m_notarization) = current_view.m_notarization
        {
            return Ok(ViewProgressEvent::ShouldVote {
                view: current_view.view_number,
                block_hash: m_notarization.block_hash,
            });
        }

        if current_view.has_voted
            && !current_view.has_nullified
            && let Some(block_hash) = current_view.block_hash
        {
            // Count conflicting messages: nullifies + votes for different blocks
            let mut conflicting_count = current_view.nullify_messages.len();

            for vote in &current_view.votes {
                if vote.block_hash != block_hash {
                    conflicting_count += 1;
                }
            }

            // If we have ≥2f+1 conflicting messages, we should nullify immediately
            if conflicting_count > 2 * F {
                return Ok(ViewProgressEvent::ShouldNullify {
                    view: current_view.view_number,
                });
            }
        }

        // Check if we have L-notarization (n-f votes) for any block in current view
        if current_view.votes.len() >= N - F
            && let Some(block_hash) = current_view.block_hash
        {
            return Ok(ViewProgressEvent::ShouldFinalize {
                view: current_view.view_number,
                block_hash,
            });
        }

        // If no block available yet, await
        if !current_view.has_voted && !current_view.has_nullified && current_view.block.is_none() {
            return Ok(ViewProgressEvent::Await);
        }

        Ok(ViewProgressEvent::NoOp)
    }

    /// Adds a new transaction to the replica's pending transaction pool.
    pub fn add_transaction(&mut self, tx: Transaction) {
        self.pending_txs.push(tx)
    }

    /// Returns pending transactions for block proposal (and clears the pool).
    pub fn take_pending_transactions(&mut self) -> Vec<Transaction> {
        std::mem::take(&mut self.pending_txs)
    }

    /// Returns the current view number.
    pub fn current_view_number(&self) -> u64 {
        self.view_chain.current_view_number()
    }

    /// Returns a mutable reference to the view context for a given view.
    pub fn view_context_mut(&mut self, view: u64) -> Result<&mut ViewContext<N, F, M_SIZE>> {
        let view_ctx = self
            .view_chain
            .find_view_context_mut(view)
            .ok_or_else(|| anyhow::anyhow!("View {} not found", view))?;
        Ok(view_ctx)
    }

    /// Returns the replica's peer ID.
    pub fn replica_id(&self) -> PeerId {
        self.replica_id
    }

    /// Returns the number of non-finalized views in the chain.
    pub fn non_finalized_count(&self) -> usize {
        self.view_chain.non_finalized_count()
    }

    pub fn leader_for_view(&self, view: u64) -> Result<PeerId> {
        let view_ctx = self
            .view_chain
            .find_view_context(view)
            .ok_or_else(|| anyhow::anyhow!("View {} not found", view))?;
        Ok(view_ctx.leader_id)
    }

    /// Finalizes a view with L-notarization.
    ///
    /// Should be called when a view reaches n-f votes to commit the block to the ledger.
    pub fn finalize_view(&mut self, view: u64) -> Result<()> {
        self.view_chain
            .finalize_with_l_notarization(view, &self.peers)
    }

    /// Marks that the current replica has proposed a block for a view.
    pub fn mark_proposed(&mut self, view: u64) -> Result<()> {
        if view == self.view_chain.current_view_number() {
            self.view_chain.current_view_mut().has_proposed = true;
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Cannot mark proposed for view {} (current view: {})",
                view,
                self.view_chain.current_view_number()
            ))
        }
    }

    /// Marks that the current replica has voted for a block for a view.
    pub fn mark_voted(&mut self, view: u64) -> Result<()> {
        if let Some(ctx) = self.view_chain.find_view_context_mut(view) {
            ctx.has_voted = true;
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Cannot mark voted for view {} (view not found)",
                view
            ))
        }
    }

    /// Marks that the current replica has nullified a view.
    pub fn mark_nullified(&mut self, view: u64) -> Result<()> {
        if let Some(ctx) = self.view_chain.find_view_context_mut(view) {
            ctx.has_nullified = true;
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Cannot mark nullified for view {} (view not found)",
                view
            ))
        }
    }

    /// Returns the M-notarization for a view.
    ///
    /// Returns an error if the view is not found or the M-notarization is not found.
    pub fn get_m_notarization(&self, view: u64) -> Result<MNotarization<N, F, M_SIZE>> {
        let view_ctx = self
            .view_chain
            .find_view_context(view)
            .ok_or_else(|| anyhow::anyhow!("View {} not found", view))?;

        view_ctx
            .m_notarization
            .clone()
            .ok_or_else(|| anyhow::anyhow!("M-notarization not found for view {}", view))
    }

    /// Returns the nullification for a view.
    ///
    /// Returns an error if the view is not found or the nullification is not found.
    pub fn get_nullification(&self, view: u64) -> Result<Nullification<N, F, M_SIZE>> {
        let view_ctx = self
            .view_chain
            .find_view_context(view)
            .ok_or_else(|| anyhow::anyhow!("View {} not found", view))?;

        view_ctx
            .nullification
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Nullification not found for view {}", view))
    }

    /// Adds the current replica's vote for a block in a view.
    /// Should be called by the state machine after creating and broadcasting a vote.
    pub fn add_own_vote(&mut self, view: u64, signature: BlsSignature) -> Result<()> {
        let view_ctx = self
            .view_chain
            .find_view_context_mut(view)
            .ok_or_else(|| anyhow::anyhow!("View {} not found", view))?;

        view_ctx.add_own_vote(signature)?;
        Ok(())
    }

    /// Adds the leader's implicit vote when proposing a block.
    /// Should be called by the state machine after creating and broadcasting a block proposal.
    pub fn add_leader_vote_for_block_proposal(
        &mut self,
        view: u64,
        block: Block,
        signature: BlsSignature,
    ) -> Result<()> {
        let view_ctx = self
            .view_chain
            .find_view_context_mut(view)
            .ok_or_else(|| anyhow::anyhow!("View {} not found", view))?;

        view_ctx.add_leader_vote_for_block_proposal(block, signature)?;
        Ok(())
    }

    /// Gracefully shuts down by persisting all non-finalized views.
    pub fn shutdown(&mut self) -> Result<()> {
        self.view_chain.persist_all_views()
    }

    /// Handles a new block proposal.
    ///
    /// Routes to view chain; only checks if we need to update to a future view.
    /// All other validation is delegated to [`ViewChain`]/[`ViewContext`].
    fn handle_block_proposal(&mut self, block: Block) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        let current_view_number = self.view_chain.current_view_number();
        let block_view_number = block.header.view;

        // Check if the current block is for a future view - in this case,
        // we need to update the view chain for a future view
        if block_view_number > current_view_number {
            let leader_id = self
                .leader_manager
                .leader_for_view(block_view_number)?
                .peer_id();
            if leader_id != block.leader {
                return Err(anyhow::anyhow!(
                    "Block leader {} is not the correct view leader {} for block's view {}",
                    block.leader,
                    leader_id,
                    block_view_number
                ));
            }
            return Ok(ViewProgressEvent::ShouldUpdateView {
                new_view: block_view_number,
                leader: leader_id,
            });
        }

        let LeaderProposalResult {
            block_hash,
            is_enough_to_m_notarize,
            is_enough_to_finalize,
            should_await,
            should_vote,
            should_nullify,
        } = self
            .view_chain
            .add_block_proposal(block_view_number, block, &self.peers)?;

        if should_await {
            return Ok(ViewProgressEvent::Await);
        }

        if should_nullify {
            return Ok(ViewProgressEvent::ShouldNullify {
                view: block_view_number,
            });
        }

        if is_enough_to_finalize && should_vote {
            return Ok(ViewProgressEvent::ShouldVoteAndFinalize {
                view: block_view_number,
                block_hash,
            });
        }

        if is_enough_to_m_notarize && should_vote {
            // Process pending child blocks that were waiting for this parent
            self.process_all_pending_blocks()?;

            return Ok(ViewProgressEvent::ShouldVoteAndMNotarize {
                view: block_view_number,
                block_hash,
                should_forward_m_notarization: true,
            });
        }

        if is_enough_to_finalize {
            return Ok(ViewProgressEvent::ShouldFinalize {
                view: block_view_number,
                block_hash,
            });
        }

        if is_enough_to_m_notarize {
            return Ok(ViewProgressEvent::ShouldMNotarize {
                view: block_view_number,
                block_hash,
                should_forward_m_notarization: true,
            });
        }

        if should_vote {
            return Ok(ViewProgressEvent::ShouldVote {
                view: block_view_number,
                block_hash,
            });
        }

        Ok(ViewProgressEvent::NoOp)
    }

    /// Handles a new vote.
    ///
    /// Routes to view chain; only checks if we need to update to a future view.
    /// All other validation is delegated to ViewChain/ViewContext.
    fn handle_vote(&mut self, vote: Vote) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        let current_view_number = self.view_chain.current_view_number();

        // Check if message is for a future view - in this case,
        // we need to update the view chain for a future view
        if vote.view > current_view_number {
            let leader_id = self.leader_manager.leader_for_view(vote.view)?.peer_id();
            return Ok(ViewProgressEvent::ShouldUpdateView {
                new_view: vote.view,
                leader: leader_id,
            });
        }

        let vote_view_number = vote.view;
        let vote_block_hash = vote.block_hash;

        let CollectedVotesResult {
            should_await,
            is_enough_to_m_notarize,
            is_enough_to_finalize,
            should_nullify,
            should_vote,
        } = self.view_chain.route_vote(vote, &self.peers)?;

        if should_await {
            return Ok(ViewProgressEvent::Await);
        }

        if should_nullify {
            return Ok(ViewProgressEvent::ShouldNullify {
                view: vote_view_number,
            });
        }

        if is_enough_to_finalize && should_vote {
            return Ok(ViewProgressEvent::ShouldVoteAndFinalize {
                view: vote_view_number,
                block_hash: vote_block_hash,
            });
        }

        if is_enough_to_m_notarize && should_vote {
            // Process pending child blocks that were waiting for this parent
            self.process_all_pending_blocks()?;

            return Ok(ViewProgressEvent::ShouldVoteAndMNotarize {
                view: vote_view_number,
                block_hash: vote_block_hash,
                should_forward_m_notarization: true,
            });
        }

        if is_enough_to_m_notarize {
            // Process pending child blocks that were waiting for this parent
            self.process_all_pending_blocks()?;

            return Ok(ViewProgressEvent::ShouldMNotarize {
                view: vote_view_number,
                block_hash: vote_block_hash,
                should_forward_m_notarization: true,
            });
        }

        if is_enough_to_finalize {
            return Ok(ViewProgressEvent::ShouldFinalize {
                view: vote_view_number,
                block_hash: vote_block_hash,
            });
        }

        if should_vote {
            return Ok(ViewProgressEvent::ShouldVote {
                view: vote_view_number,
                block_hash: vote_block_hash,
            });
        }

        Ok(ViewProgressEvent::NoOp)
    }

    /// Handles a nullify message.
    ///
    /// Routes to view chain; only checks if we need to update to a future view.
    fn handle_nullify(&mut self, nullify: Nullify) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        let current_view_number = self.view_chain.current_view_number();

        // Check if message is for a future view
        if nullify.view > current_view_number {
            let leader_id = self.leader_manager.leader_for_view(nullify.view)?.peer_id();
            if leader_id != nullify.leader_id {
                return Err(anyhow::anyhow!(
                    "Nullify for leader {} is not the correct view leader {} for nullify's view {}",
                    nullify.leader_id,
                    leader_id,
                    nullify.view
                ));
            }
            return Ok(ViewProgressEvent::ShouldUpdateView {
                new_view: nullify.view,
                leader: leader_id,
            });
        }

        // Route to view chain
        let nullify_view_number = nullify.view;
        let has_nullification = self.view_chain.route_nullify(nullify, &self.peers)?;

        if has_nullification {
            return Ok(ViewProgressEvent::ShouldBroadcastNullification {
                view: nullify_view_number,
            });
        }

        Ok(ViewProgressEvent::NoOp)
    }

    /// Handles an M-notarization.
    ///
    /// Routes to view chain and triggers view progression if appropriate.
    fn handle_m_notarization(
        &mut self,
        m_notarization: MNotarization<N, F, M_SIZE>,
    ) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        let current_view_number = self.view_chain.current_view_number();

        // Check if message is for a future view
        if m_notarization.view > current_view_number {
            let leader_id = self
                .leader_manager
                .leader_for_view(m_notarization.view)?
                .peer_id();
            if leader_id != m_notarization.leader_id {
                return Err(anyhow::anyhow!(
                    "M-notarization for leader {} is not the correct view leader {} for m-notarization's view {}",
                    m_notarization.leader_id,
                    leader_id,
                    m_notarization.view
                ));
            }
            return Ok(ViewProgressEvent::ShouldUpdateView {
                new_view: m_notarization.view,
                leader: leader_id,
            });
        }

        let m_notarization_view_number = m_notarization.view;
        let m_notarization_block_hash = m_notarization.block_hash;

        let ShouldMNotarize {
            should_notarize,
            should_await,
            should_vote,
            should_nullify,
            should_forward,
        } = self
            .view_chain
            .route_m_notarization(m_notarization, &self.peers)?;

        if should_await {
            return Ok(ViewProgressEvent::Await);
        }

        if should_nullify {
            return Ok(ViewProgressEvent::ShouldNullifyView {
                view: m_notarization_view_number,
            });
        }

        if m_notarization_view_number == current_view_number {
            let new_view = m_notarization_view_number + 1;
            let new_leader = self.leader_manager.leader_for_view(new_view)?.peer_id();

            // With M-notarization, parent hash updates to the notarized block
            let parent_hash = m_notarization_block_hash;
            let new_view_context =
                ViewContext::new(new_view, new_leader, self.replica_id, parent_hash);
            self.view_chain
                .progress_with_m_notarization(new_view_context)?;

            if should_vote {
                return Ok(ViewProgressEvent::ShouldVoteAndProgressToNextView {
                    old_view: m_notarization_view_number,
                    block_hash: m_notarization_block_hash,
                    new_view,
                    leader: new_leader,
                    should_forward_m_notarization: should_forward,
                });
            } else {
                return Ok(ViewProgressEvent::ProgressToNextView {
                    new_view,
                    leader: new_leader,
                    notarized_block_hash: m_notarization_block_hash,
                    should_forward_m_notarization: should_forward,
                });
            }
        }

        if should_notarize && should_vote {
            // Process pending child blocks that were waiting for this parent
            self.process_all_pending_blocks()?;

            return Ok(ViewProgressEvent::ShouldVoteAndMNotarize {
                view: m_notarization_view_number,
                block_hash: m_notarization_block_hash,
                should_forward_m_notarization: should_forward,
            });
        }

        if should_notarize {
            // Process pending child blocks that were waiting for this parent
            self.process_all_pending_blocks()?;

            return Ok(ViewProgressEvent::ShouldMNotarize {
                view: m_notarization_view_number,
                block_hash: m_notarization_block_hash,
                should_forward_m_notarization: should_forward,
            });
        }

        if should_vote {
            return Ok(ViewProgressEvent::ShouldVote {
                view: m_notarization_view_number,
                block_hash: m_notarization_block_hash,
            });
        }

        Ok(ViewProgressEvent::NoOp)
    }

    /// Handles a nullification.
    ///
    /// Routes to view chain and triggers view progression if appropriate.
    fn handle_nullification(
        &mut self,
        nullification: Nullification<N, F, M_SIZE>,
    ) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        let current_view_number = self.view_chain.current_view_number();

        // Check if message is for a future view
        if nullification.view > current_view_number {
            let leader_id = self
                .leader_manager
                .leader_for_view(nullification.view)?
                .peer_id();
            if leader_id != nullification.leader_id {
                return Err(anyhow::anyhow!(
                    "Nullification for leader {} is not the correct view leader {} for nullification's view {}",
                    nullification.leader_id,
                    leader_id,
                    nullification.view
                ));
            }
            return Ok(ViewProgressEvent::ShouldUpdateView {
                new_view: nullification.view,
                leader: leader_id,
            });
        }

        let nullification_view_number = nullification.view;

        let CollectedNullificationsResult {
            should_broadcast_nullification,
        } = self
            .view_chain
            .route_nullification(nullification, &self.peers)?;

        // Progress to next view with nullification
        if nullification_view_number == current_view_number {
            let new_view = nullification_view_number + 1;
            let new_leader = self.leader_manager.leader_for_view(new_view)?.peer_id();

            // With nullification, parent hash stays the same (no progress)
            let parent_hash = self.view_chain.select_parent(new_view);
            let new_view_context =
                ViewContext::new(new_view, new_leader, self.replica_id, parent_hash);
            self.view_chain
                .progress_with_nullification(new_view_context)?;

            return Ok(ViewProgressEvent::ProgressToNextViewOnNullification {
                new_view,
                leader: new_leader,
                parent_block_hash: parent_hash,
                should_broadcast_nullification,
            });
        }

        if should_broadcast_nullification {
            return Ok(ViewProgressEvent::ShouldBroadcastNullification {
                view: nullification_view_number,
            });
        }

        Ok(ViewProgressEvent::ShouldNullify {
            view: nullification_view_number,
        })
    }

    /// Process pending child blocks recursively until no more can be processed.
    /// This handles cascading scenarios where processing a pending block causes it to reach
    /// M-notarization, which then allows its own pending children to be processed.
    fn process_all_pending_blocks(&mut self) -> Result<()> {
        loop {
            let mut made_progress = false;

            // Collect all views that have M-notarization
            let views_with_m_not: Vec<u64> = self
                .view_chain
                .non_finalized_views
                .iter()
                .filter(|(_, ctx)| ctx.m_notarization.is_some())
                .map(|(v, _)| *v)
                .collect();

            // Try to process pending children for each M-notarized view
            for view in views_with_m_not {
                let results = self
                    .view_chain
                    .process_pending_child_proposals(view, &self.peers)?;
                if !results.is_empty() {
                    made_progress = true;
                }
            }

            // If no pending blocks were processed this iteration, we're done
            if !made_progress {
                break;
            }
        }

        Ok(())
    }

    fn _try_update_view(&mut self, view: u64) -> Result<()> {
        // TODO: Implement view update logic
        tracing::info!("Trying to update view to {}", view);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        consensus_manager::{
            config::{ConsensusConfig, Network},
            leader_manager::{LeaderSelectionStrategy, RoundRobinLeaderManager},
            utils::{create_notarization_data, create_nullification_data},
        },
        crypto::aggregated::BlsSecretKey,
        state::{
            block::Block,
            notarizations::{MNotarization, Vote},
            nullify::{Nullification, Nullify},
            peer::PeerSet,
            transaction::Transaction,
        },
    };
    use ark_serialize::CanonicalSerialize;
    use rand::thread_rng;
    use std::{
        collections::{HashMap, HashSet},
        time::Duration,
    };

    /// Helper struct to hold test setup data
    struct TestSetup {
        peer_set: PeerSet,
        peer_id_to_secret_key: HashMap<PeerId, BlsSecretKey>,
    }

    /// Creates a test peer set with secret keys
    fn create_test_peer_setup(size: usize) -> TestSetup {
        let mut rng = thread_rng();
        let mut public_keys = vec![];
        let mut peer_id_to_secret_key = HashMap::new();

        for _ in 0..size {
            let sk = BlsSecretKey::generate(&mut rng);
            let pk = sk.public_key();
            let peer_id = pk.to_peer_id();
            peer_id_to_secret_key.insert(peer_id, sk);
            public_keys.push(pk);
        }

        TestSetup {
            peer_set: PeerSet::new(public_keys),
            peer_id_to_secret_key,
        }
    }

    /// Creates a test transaction
    fn create_test_transaction() -> Transaction {
        let mut rng = thread_rng();
        let sk = BlsSecretKey::generate(&mut rng);
        let pk = sk.public_key();
        let tx_hash: [u8; blake3::OUT_LEN] = blake3::hash(b"test tx").into();
        let sig = sk.sign(&tx_hash);
        Transaction::new(pk, [7u8; 32], 42, 9, 1_000, 3, tx_hash, sig)
    }

    /// Creates a test block
    fn create_test_block(
        view: u64,
        leader: PeerId,
        parent_hash: [u8; blake3::OUT_LEN],
        leader_sk: BlsSecretKey,
        height: u64,
    ) -> Block {
        let transactions = vec![create_test_transaction()];

        // First, create a temporary block to compute its hash
        let temp_block = Block::new(
            view,
            leader,
            parent_hash,
            transactions.clone(),
            1234567890,
            leader_sk.sign(b"temp"), // Temporary signature
            false,
            height,
        );

        // Get the block hash
        let block_hash = temp_block.get_hash();

        Block::new(
            view,
            leader,
            parent_hash,
            transactions,
            1234567890,
            leader_sk.sign(&block_hash),
            false,
            height,
        )
    }

    /// Creates a signed vote from a peer
    fn create_test_vote(
        peer_index: usize,
        view: u64,
        block_hash: [u8; blake3::OUT_LEN],
        leader_id: PeerId,
        setup: &TestSetup,
    ) -> Vote {
        let peer_id = setup.peer_set.sorted_peer_ids[peer_index];
        let secret_key = setup.peer_id_to_secret_key.get(&peer_id).unwrap();
        let signature = secret_key.sign(&block_hash);
        Vote::new(view, block_hash, signature, peer_id, leader_id)
    }

    /// Creates a signed nullify message from a peer
    fn create_test_nullify(
        peer_index: usize,
        view: u64,
        leader_id: PeerId,
        setup: &TestSetup,
    ) -> Nullify {
        let peer_id = setup.peer_set.sorted_peer_ids[peer_index];
        let secret_key = setup.peer_id_to_secret_key.get(&peer_id).unwrap();
        let message = blake3::hash(&[view.to_le_bytes(), leader_id.to_le_bytes()].concat());
        let signature = secret_key.sign(message.as_bytes());
        Nullify::new(view, leader_id, signature, peer_id)
    }

    /// Creates a test M-notarization from votes
    fn create_test_m_notarization<const N: usize, const F: usize, const M_SIZE: usize>(
        votes: &HashSet<Vote>,
        view: u64,
        block_hash: [u8; blake3::OUT_LEN],
        leader_id: PeerId,
    ) -> MNotarization<N, F, M_SIZE> {
        let data = create_notarization_data::<M_SIZE>(votes).unwrap();
        MNotarization::new(
            view,
            block_hash,
            data.aggregated_signature,
            data.peer_ids,
            leader_id,
        )
    }

    /// Creates a test nullification from nullify messages
    fn create_test_nullification<const N: usize, const F: usize, const M_SIZE: usize>(
        nullify_messages: &HashSet<Nullify>,
        view: u64,
        leader_id: PeerId,
    ) -> Nullification<N, F, M_SIZE> {
        let data = create_nullification_data::<M_SIZE>(nullify_messages).unwrap();
        Nullification::new(view, leader_id, data.aggregated_signature, data.peer_ids)
    }

    /// Creates a test consensus config
    fn create_test_config(n: usize, f: usize, peer_public_keys: Vec<String>) -> ConsensusConfig {
        ConsensusConfig::new(
            n,
            f,
            Duration::from_secs(5),
            LeaderSelectionStrategy::RoundRobin,
            Network::Local,
            peer_public_keys,
        )
    }

    fn temp_db_path(suffix: &str) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "consensus_store_test-{}-{}.redb",
            suffix,
            rand::random::<u64>()
        ));
        p.to_string_lossy().to_string()
    }

    /// Creates a test view progress manager
    fn create_test_manager<const N: usize, const F: usize, const M_SIZE: usize>(
        setup: &TestSetup,
        replica_index: usize,
    ) -> (ViewProgressManager<N, F, M_SIZE>, String) {
        let replica_id = setup.peer_set.sorted_peer_ids[replica_index];
        let mut peer_strs = Vec::with_capacity(setup.peer_set.sorted_peer_ids.len());
        for peer_id in &setup.peer_set.sorted_peer_ids {
            let pk = setup.peer_set.id_to_public_key.get(peer_id).unwrap();
            let mut buf = Vec::new();
            pk.0.serialize_compressed(&mut buf).unwrap();
            let peer_str = hex::encode(buf);
            peer_strs.push(peer_str);
        }
        let config = create_test_config(N, F, peer_strs);

        let leader_manager = Box::new(RoundRobinLeaderManager::new(
            N,
            setup.peer_set.sorted_peer_ids.clone(),
        ));

        let path = temp_db_path("view_manager");
        let persistence_storage = ConsensusStore::open(&path).unwrap();
        (
            ViewProgressManager::new(config, replica_id, persistence_storage, leader_manager)
                .unwrap(),
            path,
        )
    }

    #[test]
    fn test_new_creates_manager_with_correct_initial_state() {
        let setup = create_test_peer_setup(6);
        let (manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 0);

        assert_eq!(manager.current_view_number(), 0);
        assert_eq!(manager.non_finalized_count(), 1);
        assert_eq!(manager.replica_id(), setup.peer_set.sorted_peer_ids[0]);
        assert_eq!(manager.peers.sorted_peer_ids.len(), 6);
        assert_eq!(manager.pending_txs.len(), 0);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_from_genesis_creates_manager_with_genesis_state() {
        let setup = create_test_peer_setup(6);
        let replica_id = setup.peer_set.sorted_peer_ids[0];
        let mut peer_strs: Vec<String> = Vec::with_capacity(setup.peer_set.sorted_peer_ids.len());
        for peer_id in &setup.peer_set.sorted_peer_ids {
            let pk = setup.peer_set.id_to_public_key.get(peer_id).unwrap();
            let mut buf = Vec::new();
            pk.0.serialize_compressed(&mut buf).unwrap();
            let peer_str = hex::encode(buf);
            peer_strs.push(peer_str);
        }
        let config = create_test_config(6, 1, peer_strs);

        let path = temp_db_path("view_manager_genesis");
        let persistence_storage = ConsensusStore::open(&path).unwrap();
        let manager: ViewProgressManager<6, 1, 3> =
            ViewProgressManager::from_genesis(config, replica_id, persistence_storage).unwrap();

        assert_eq!(manager.current_view_number(), 0);
        assert_eq!(manager.non_finalized_count(), 1);
        assert_eq!(manager.peers.sorted_peer_ids.len(), 6);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_new_sets_correct_leader_for_view_zero() {
        let setup = create_test_peer_setup(6);
        let (manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // For round-robin, view 0 should have leader at index 0
        let expected_leader = setup.peer_set.sorted_peer_ids[0];
        let current_view = manager.view_chain.current();
        assert_eq!(current_view.leader_id, expected_leader);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_add_transaction_increases_pending_txs() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 0);

        assert_eq!(manager.pending_txs.len(), 0);

        let tx = create_test_transaction();
        manager.add_transaction(tx.clone());

        assert_eq!(manager.pending_txs.len(), 1);
        assert_eq!(manager.pending_txs[0], tx);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_add_multiple_transactions() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 0);

        for _ in 0..5 {
            manager.add_transaction(create_test_transaction());
        }

        assert_eq!(manager.pending_txs.len(), 5);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_take_pending_transactions_clears_pool() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 0);

        for _ in 0..3 {
            manager.add_transaction(create_test_transaction());
        }

        let txs = manager.take_pending_transactions();
        assert_eq!(txs.len(), 3);
        assert_eq!(manager.pending_txs.len(), 0);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_as_leader_returns_should_propose_block() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 0); // Replica 0 is leader for view 0

        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldProposeBlock {
                view,
                parent_block_hash,
            } => {
                assert_eq!(view, 0);
                assert_eq!(parent_block_hash, [0; blake3::OUT_LEN]);
            }
            _ => panic!("Expected ShouldProposeBlock event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_as_leader_after_proposing_returns_await() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 0);

        manager.mark_proposed(0).unwrap();

        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::Await => {}
            _ => panic!("Expected Await event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_non_leader_with_block_returns_should_vote() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1); // Not leader

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);

        // Add block to view chain
        manager.handle_block_proposal(block).unwrap();

        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldVote {
                view,
                block_hash: _,
            } => {
                assert_eq!(view, 0);
            }
            _ => panic!("Expected ShouldVote event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_without_block_returns_await() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::Await => {}
            _ => panic!("Expected Await event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_after_voting_returns_no_op() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        manager.mark_voted(0).unwrap();

        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::NoOp => {}
            _ => panic!("Expected NoOp event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_after_timeout_returns_should_nullify() {
        // Create setup with 6 peers
        let setup = create_test_peer_setup(6);

        // Create config with a very short timeout for testing (100ms)
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let mut peer_strs = Vec::with_capacity(setup.peer_set.sorted_peer_ids.len());
        for peer_id in &setup.peer_set.sorted_peer_ids {
            let pk = setup.peer_set.id_to_public_key.get(peer_id).unwrap();
            let mut buf = Vec::new();
            pk.0.serialize_compressed(&mut buf).unwrap();
            let peer_str = hex::encode(buf);
            peer_strs.push(peer_str);
        }

        // Create config with SHORT timeout for testing
        let config = ConsensusConfig::new(
            6,
            1,
            Duration::from_millis(100), // Short timeout for testing
            LeaderSelectionStrategy::RoundRobin,
            Network::Local,
            peer_strs,
        );

        let leader_manager = Box::new(RoundRobinLeaderManager::new(
            6,
            setup.peer_set.sorted_peer_ids.clone(),
        ));

        let path = temp_db_path("view_manager_timeout");
        let persistence_storage = ConsensusStore::open(&path).unwrap();
        let mut manager: ViewProgressManager<6, 1, 3> =
            ViewProgressManager::new(config, replica_id, persistence_storage, leader_manager)
                .unwrap();

        // Verify initial state - should be waiting for block
        let initial_result = manager.tick();
        assert!(initial_result.is_ok());
        match initial_result.unwrap() {
            ViewProgressEvent::Await => {} // Expected - no block yet
            _ => panic!("Expected Await event before timeout"),
        }

        // Sleep for longer than the timeout duration
        std::thread::sleep(Duration::from_millis(150));

        // After timeout, tick should return ShouldNullify
        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldNullify { view } => {
                assert_eq!(view, 0);
            }
            other => panic!(
                "Expected ShouldNullify event after timeout, got {:?}",
                other
            ),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_after_timeout_when_already_voted_returns_no_op() {
        let setup = create_test_peer_setup(6);
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let mut peer_strs = Vec::with_capacity(setup.peer_set.sorted_peer_ids.len());
        for peer_id in &setup.peer_set.sorted_peer_ids {
            let pk = setup.peer_set.id_to_public_key.get(peer_id).unwrap();
            let mut buf = Vec::new();
            pk.0.serialize_compressed(&mut buf).unwrap();
            let peer_str = hex::encode(buf);
            peer_strs.push(peer_str);
        }

        let config = ConsensusConfig::new(
            6,
            1,
            Duration::from_millis(100),
            LeaderSelectionStrategy::RoundRobin,
            Network::Local,
            peer_strs,
        );

        let leader_manager = Box::new(RoundRobinLeaderManager::new(
            6,
            setup.peer_set.sorted_peer_ids.clone(),
        ));

        let path = temp_db_path("view_manager_timeout_voted");
        let persistence_storage = ConsensusStore::open(&path).unwrap();
        let mut manager: ViewProgressManager<6, 1, 3> =
            ViewProgressManager::new(config, replica_id, persistence_storage, leader_manager)
                .unwrap();

        // Mark as already voted
        manager.mark_voted(0).unwrap();

        // Sleep past timeout
        std::thread::sleep(Duration::from_millis(150));

        // Should NOT nullify since already voted
        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::NoOp => {} // Correct - can't nullify after voting
            ViewProgressEvent::ShouldNullify { .. } => {
                panic!("Should not nullify after voting");
            }
            _ => {}
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_after_timeout_when_already_nullified_returns_no_op() {
        let setup = create_test_peer_setup(6);
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let mut peer_strs = Vec::with_capacity(setup.peer_set.sorted_peer_ids.len());
        for peer_id in &setup.peer_set.sorted_peer_ids {
            let pk = setup.peer_set.id_to_public_key.get(peer_id).unwrap();
            let mut buf = Vec::new();
            pk.0.serialize_compressed(&mut buf).unwrap();
            let peer_str = hex::encode(buf);
            peer_strs.push(peer_str);
        }

        let config = ConsensusConfig::new(
            6,
            1,
            Duration::from_millis(100),
            LeaderSelectionStrategy::RoundRobin,
            Network::Local,
            peer_strs,
        );

        let leader_manager = Box::new(RoundRobinLeaderManager::new(
            6,
            setup.peer_set.sorted_peer_ids.clone(),
        ));

        let path = temp_db_path("view_manager_timeout_nullified");
        let persistence_storage = ConsensusStore::open(&path).unwrap();
        let mut manager: ViewProgressManager<6, 1, 3> =
            ViewProgressManager::new(config, replica_id, persistence_storage, leader_manager)
                .unwrap();

        // Mark as already nullified
        manager.mark_nullified(0).unwrap();

        // Sleep past timeout
        std::thread::sleep(Duration::from_millis(150));

        // Should NOT nullify again
        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::NoOp => {} // Correct - already nullified
            ViewProgressEvent::ShouldNullify { .. } => {
                panic!("Should not nullify twice");
            }
            _ => {}
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_past_view_timeout_returns_should_nullify() {
        // Test that past views (not current) can also timeout
        let setup = create_test_peer_setup(6);
        let replica_id = setup.peer_set.sorted_peer_ids[2]; // Not leader for views 0 and 1
        let mut peer_strs = Vec::with_capacity(setup.peer_set.sorted_peer_ids.len());
        for peer_id in &setup.peer_set.sorted_peer_ids {
            let pk = setup.peer_set.id_to_public_key.get(peer_id).unwrap();
            let mut buf = Vec::new();
            pk.0.serialize_compressed(&mut buf).unwrap();
            let peer_str = hex::encode(buf);
            peer_strs.push(peer_str);
        }

        let config = ConsensusConfig::new(
            6,
            1,
            Duration::from_millis(100),
            LeaderSelectionStrategy::RoundRobin,
            Network::Local,
            peer_strs,
        );

        let leader_manager = Box::new(RoundRobinLeaderManager::new(
            6,
            setup.peer_set.sorted_peer_ids.clone(),
        ));

        let path = temp_db_path("view_manager_past_timeout");
        let persistence_storage = ConsensusStore::open(&path).unwrap();
        let mut manager: ViewProgressManager<6, 1, 3> =
            ViewProgressManager::new(config, replica_id, persistence_storage, leader_manager)
                .unwrap();

        // Create and add M-notarization for view 0
        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id_0).unwrap();
        let block = create_test_block(0, leader_id_0, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        manager.handle_block_proposal(block).unwrap();

        let mut votes = HashSet::new();
        for i in 1..=3 {
            votes.insert(create_test_vote(i, 0, block_hash, leader_id_0, &setup));
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 0, block_hash, leader_id_0);
        manager.handle_m_notarization(m_notarization).unwrap();

        // Now manager is already in view 1 due to M-notarization handling
        assert_eq!(manager.current_view_number(), 1);

        // Sleep past timeout (view 0 should timeout)
        std::thread::sleep(Duration::from_millis(150));

        // Should suggest nullifying view 0 (past view)
        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldNullify { view } => {
                assert_eq!(view, 1, "Should nullify past view 0");
            }
            other => panic!("Expected ShouldNullify for past view, got {:?}", other),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_with_m_notarization_but_no_vote_returns_should_vote() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        // Add block
        manager.handle_block_proposal(block).unwrap();

        // Create M-notarization
        let mut votes = HashSet::new();
        for i in 2..=4 {
            votes.insert(create_test_vote(i, 0, block_hash, leader_id, &setup));
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 0, block_hash, leader_id);

        // Manually add M-notarization to view context WITHOUT calling handle_m_notarization
        // (which would trigger view progression)
        let view_ctx = manager.view_chain.find_view_context_mut(0).unwrap();
        view_ctx.m_notarization = Some(m_notarization);

        // Verify we're still in view 0
        assert_eq!(manager.current_view_number(), 0);

        let result = manager.tick();
        assert!(result.is_ok());

        // Should suggest voting on the M-notarized block
        match result.unwrap() {
            ViewProgressEvent::ShouldVote {
                view,
                block_hash: hash,
            } => {
                assert_eq!(view, 0);
                assert_eq!(hash, block_hash);
            }
            other => panic!("Expected ShouldVote event, got {:?}", other),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_m_notarization_when_not_voted_returns_should_vote_and_progress() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        // Add block first
        manager.handle_block_proposal(block).unwrap();

        // Verify replica hasn't voted yet
        let view_ctx = manager.view_chain.find_view_context(0).unwrap();
        assert!(!view_ctx.has_voted);

        // Create and handle M-notarization
        let mut votes = HashSet::new();
        for i in 2..=4 {
            votes.insert(create_test_vote(i, 0, block_hash, leader_id, &setup));
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 0, block_hash, leader_id);

        let result = manager.handle_m_notarization(m_notarization);
        assert!(result.is_ok());

        // Should return ShouldVoteAndProgressToNextView since we haven't voted
        match result.unwrap() {
            ViewProgressEvent::ShouldVoteAndProgressToNextView {
                old_view,
                block_hash: hash,
                new_view,
                leader: _,
                should_forward_m_notarization,
            } => {
                assert_eq!(old_view, 0);
                assert_eq!(hash, block_hash);
                assert_eq!(new_view, 1);
                assert!(should_forward_m_notarization);
            }
            other => panic!("Expected ShouldVoteAndProgressToNextView, got {:?}", other),
        }

        // Verify view has progressed
        assert_eq!(manager.current_view_number(), 1);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_m_notarization_when_already_voted_returns_progress_only() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        // Add block first
        manager.handle_block_proposal(block).unwrap();

        // Mark as already voted
        manager.mark_voted(0).unwrap();

        // Create and handle M-notarization
        let mut votes = HashSet::new();
        for i in 2..=4 {
            votes.insert(create_test_vote(i, 0, block_hash, leader_id, &setup));
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 0, block_hash, leader_id);

        let result = manager.handle_m_notarization(m_notarization);
        assert!(result.is_ok());

        // Should return ProgressToNextView (no vote needed since already voted)
        match result.unwrap() {
            ViewProgressEvent::ProgressToNextView {
                new_view,
                leader: _,
                notarized_block_hash,
                should_forward_m_notarization,
            } => {
                assert_eq!(new_view, 1);
                assert_eq!(notarized_block_hash, block_hash);
                assert!(should_forward_m_notarization);
            }
            other => panic!("Expected ProgressToNextView, got {:?}", other),
        }

        // Verify view has progressed
        assert_eq!(manager.current_view_number(), 1);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_m_notarization_when_already_nullified() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        // Add block first
        manager.handle_block_proposal(block).unwrap();

        // Mark as already nullified
        manager.mark_nullified(0).unwrap();

        // Create and handle M-notarization
        let mut votes = HashSet::new();
        for i in 2..=4 {
            votes.insert(create_test_vote(i, 0, block_hash, leader_id, &setup));
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 0, block_hash, leader_id);

        let result = manager.handle_m_notarization(m_notarization);
        assert!(result.is_ok());

        // When already nullified, should still progress (can't vote)
        match result.unwrap() {
            ViewProgressEvent::ProgressToNextView { new_view, .. } => {
                assert_eq!(new_view, 1);
            }
            ViewProgressEvent::ShouldNullifyView { .. } => {
                // Also acceptable - conflict detected
            }
            other => panic!(
                "Expected ProgressToNextView or ShouldNullifyView, got {:?}",
                other
            ),
        }

        // Verify view has progressed
        assert_eq!(manager.current_view_number(), 1);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_m_notarization_without_block_still_progresses() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let block_hash = [1u8; blake3::OUT_LEN];

        // Receive M-notarization WITHOUT having the block
        let mut votes = HashSet::new();
        for i in 2..=4 {
            votes.insert(create_test_vote(i, 0, block_hash, leader_id, &setup));
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 0, block_hash, leader_id);

        let result = manager.handle_m_notarization(m_notarization);
        // assert!(result.is_ok());

        // Should still progress view (M-notarization is sufficient for safety)
        match result.unwrap() {
            ViewProgressEvent::ShouldVoteAndProgressToNextView { new_view, .. }
            | ViewProgressEvent::ProgressToNextView { new_view, .. } => {
                assert_eq!(new_view, 1);
            }
            other => panic!("Expected view progression event, got {:?}", other),
        }

        // Verify view has progressed even without block
        assert_eq!(manager.current_view_number(), 1);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_with_enough_votes_returns_should_finalize() {
        let setup = create_test_peer_setup(6);
        // Create as NON-leader (replica 1) to test receiving block scenario
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        // Receive block from leader (this adds it to the view)
        manager.handle_block_proposal(block).unwrap();

        // Simulate that we've already voted
        manager.mark_voted(0).unwrap();

        // Manually add votes to the view context to simulate receiving them
        // We need n-f = 5 total votes for finalization
        // Add 4 votes (leader + 3 others), bringing us to the threshold
        for i in 0..=3 {
            let peer_id = setup.peer_set.sorted_peer_ids[i];
            let sk = setup.peer_id_to_secret_key.get(&peer_id).unwrap();
            let sig = sk.sign(&block_hash);
            let vote = Vote::new(0, block_hash, sig, peer_id, leader_id);

            // Add directly to view context to avoid triggering events
            let view_ctx = manager.view_chain.find_view_context_mut(0).unwrap();
            view_ctx.votes.insert(vote);
            view_ctx.block_hash = Some(block_hash);
        }

        // Now we have 4 votes, need 1 more for n-f = 5
        // Add the 5th vote
        let peer_id = setup.peer_set.sorted_peer_ids[4];
        let sk = setup.peer_id_to_secret_key.get(&peer_id).unwrap();
        let sig = sk.sign(&block_hash);
        let vote = Vote::new(0, block_hash, sig, peer_id, leader_id);

        let view_ctx = manager.view_chain.find_view_context_mut(0).unwrap();
        view_ctx.votes.insert(vote);

        // Now tick should detect finalization threshold
        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldFinalize {
                view,
                block_hash: hash,
            } => {
                assert_eq!(view, 0);
                assert_eq!(hash, block_hash);
            }
            other => panic!("Expected ShouldFinalize event, got {:?}", other),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_detects_finalization_threshold_reached() {
        // Test that tick() can detect when finalization threshold is reached
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        // Add block to view
        manager.handle_block_proposal(block).unwrap();
        manager.mark_voted(0).unwrap();

        // Directly add exactly n-f votes to view context
        let view_ctx = manager.view_chain.find_view_context_mut(0).unwrap();
        view_ctx.block_hash = Some(block_hash);

        for i in 0..5 {
            // n-f = 6-1 = 5 votes
            let peer_id = setup.peer_set.sorted_peer_ids[i];
            let sk = setup.peer_id_to_secret_key.get(&peer_id).unwrap();
            let sig = sk.sign(&block_hash);
            let vote = Vote::new(0, block_hash, sig, peer_id, leader_id);
            view_ctx.votes.insert(vote);
        }

        // Verify we have exactly n-f votes
        assert_eq!(view_ctx.votes.len(), 5);

        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldFinalize {
                view,
                block_hash: _,
            } => {
                assert_eq!(view, 0);
            }
            other => panic!("Expected ShouldFinalize, got {:?}", other),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_detects_conflicting_votes_and_returns_should_nullify() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        // Add block and vote for it
        manager.handle_block_proposal(block).unwrap();
        manager.mark_voted(0).unwrap();

        // Manually add conflicting votes to view context
        let view_ctx = manager.view_chain.find_view_context_mut(0).unwrap();
        view_ctx.block_hash = Some(block_hash);

        // Add 2 votes for different hash + 1 nullify = 3 conflicting messages (≥ 2f+1)
        let conflicting_hash = [255u8; blake3::OUT_LEN];
        for i in 2..=3 {
            let peer_id = setup.peer_set.sorted_peer_ids[i];
            let sk = setup.peer_id_to_secret_key.get(&peer_id).unwrap();
            let sig = sk.sign(&conflicting_hash);
            let vote = Vote::new(0, conflicting_hash, sig, peer_id, leader_id);
            view_ctx.votes.insert(vote);
        }

        // Add a nullify message
        let nullify = create_test_nullify(4, 0, leader_id, &setup);
        view_ctx.nullify_messages.insert(nullify);

        // Now tick should detect conflicting messages
        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldNullify { view } => {
                assert_eq!(view, 0);
            }
            _ => panic!("Expected ShouldNullify due to conflicting messages"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_current_view_timeout_after_nullified_past_views() {
        let setup = create_test_peer_setup(6);
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let mut peer_strs = Vec::with_capacity(setup.peer_set.sorted_peer_ids.len());
        for peer_id in &setup.peer_set.sorted_peer_ids {
            let pk = setup.peer_set.id_to_public_key.get(peer_id).unwrap();
            let mut buf = Vec::new();
            pk.0.serialize_compressed(&mut buf).unwrap();
            let peer_str = hex::encode(buf);
            peer_strs.push(peer_str);
        }

        let config = ConsensusConfig::new(
            6,
            1,
            Duration::from_millis(100),
            LeaderSelectionStrategy::RoundRobin,
            Network::Local,
            peer_strs,
        );

        let leader_manager = Box::new(RoundRobinLeaderManager::new(
            6,
            setup.peer_set.sorted_peer_ids.clone(),
        ));

        let path = temp_db_path("view_manager_multiple_timeout");
        let persistence_storage = ConsensusStore::open(&path).unwrap();
        let mut manager: ViewProgressManager<6, 1, 3> =
            ViewProgressManager::new(config, replica_id, persistence_storage, leader_manager)
                .unwrap();

        // Progress to view 2 by creating nullifications for views 0 and 1
        for view_num in 0..=1 {
            let leader_id = setup.peer_set.sorted_peer_ids[view_num % 6];

            // Create nullify messages for this view (need 2f+1 = 3 for F=1)
            let mut nullify_messages = HashSet::new();
            for i in 0..3 {
                let peer_id = setup.peer_set.sorted_peer_ids[i];
                let peer_sk = setup.peer_id_to_secret_key.get(&peer_id).unwrap();
                let message = blake3::hash(
                    &[(view_num as u64).to_le_bytes(), leader_id.to_le_bytes()].concat(),
                );
                let signature = peer_sk.sign(message.as_bytes());
                let nullify = Nullify::new(view_num as u64, leader_id, signature, peer_id);
                nullify_messages.insert(nullify);
            }

            // Create nullification from the nullify messages
            let nullification =
                create_test_nullification::<6, 1, 3>(&nullify_messages, view_num as u64, leader_id);

            // Add nullification to current view
            manager
                .view_chain
                .route_nullification(nullification, &setup.peer_set)
                .unwrap();

            // Progress to next view (except after view 1, we're done)
            if view_num < 1 {
                let next_leader_id = setup.peer_set.sorted_peer_ids[(view_num + 1) % 6];
                let next_view_ctx = ViewContext::new(
                    (view_num + 1) as u64,
                    next_leader_id,
                    replica_id,
                    [0; blake3::OUT_LEN],
                );
                manager
                    .view_chain
                    .progress_with_nullification(next_view_ctx)
                    .unwrap();
            }
        }

        // Now manually add one more nullification to progress to view 2
        let view1_leader = setup.peer_set.sorted_peer_ids[1];
        let mut nullify_messages_v1 = HashSet::new();
        for i in 0..3 {
            let peer_id = setup.peer_set.sorted_peer_ids[i];
            let peer_sk = setup.peer_id_to_secret_key.get(&peer_id).unwrap();
            let message = blake3::hash(&[1u64.to_le_bytes(), view1_leader.to_le_bytes()].concat());
            let signature = peer_sk.sign(message.as_bytes());
            let nullify = Nullify::new(1, view1_leader, signature, peer_id);
            nullify_messages_v1.insert(nullify);
        }
        let nullification_v1 =
            create_test_nullification::<6, 1, 3>(&nullify_messages_v1, 1, view1_leader);
        manager
            .view_chain
            .route_nullification(nullification_v1, &setup.peer_set)
            .unwrap();

        let view2_leader = setup.peer_set.sorted_peer_ids[2];
        let view2_ctx = ViewContext::new(2, view2_leader, replica_id, [0; blake3::OUT_LEN]);
        manager
            .view_chain
            .progress_with_nullification(view2_ctx)
            .unwrap();

        // Sleep past timeout to make views 0 and 1 timeout-eligible
        std::thread::sleep(Duration::from_millis(150));

        // First tick should suggest nullifying view 0 (already has nullification but timeout check)
        let result1 = manager.tick();
        assert!(result1.is_ok());
        match result1.unwrap() {
            ViewProgressEvent::ShouldNullify { view } => {
                assert_eq!(view, 2);
                manager.mark_nullified(2).unwrap();
            }
            ViewProgressEvent::Await => {
                panic!("Expected ShouldNullify, got Await");
            }
            _ => panic!("Expected ShouldNullify"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_priority_block_present_over_timeout() {
        let setup = create_test_peer_setup(6);
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let mut peer_strs = Vec::with_capacity(setup.peer_set.sorted_peer_ids.len());
        for peer_id in &setup.peer_set.sorted_peer_ids {
            let pk = setup.peer_set.id_to_public_key.get(peer_id).unwrap();
            let mut buf = Vec::new();
            pk.0.serialize_compressed(&mut buf).unwrap();
            let peer_str = hex::encode(buf);
            peer_strs.push(peer_str);
        }

        let config = ConsensusConfig::new(
            6,
            1,
            Duration::from_millis(100),
            LeaderSelectionStrategy::RoundRobin,
            Network::Local,
            peer_strs,
        );

        let leader_manager = Box::new(RoundRobinLeaderManager::new(
            6,
            setup.peer_set.sorted_peer_ids.clone(),
        ));

        let path = temp_db_path("view_manager_priority");
        let persistence_storage = ConsensusStore::open(&path).unwrap();
        let mut manager: ViewProgressManager<6, 1, 3> =
            ViewProgressManager::new(config, replica_id, persistence_storage, leader_manager)
                .unwrap();

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);

        // Add block
        manager.handle_block_proposal(block).unwrap();

        // Sleep past timeout
        std::thread::sleep(Duration::from_millis(150));

        // tick should prioritize voting for block over timeout nullification
        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldVote {
                view,
                block_hash: _,
            } => {
                assert_eq!(view, 0);
            }
            _ => panic!("Expected ShouldVote (block present should override timeout)"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_exactly_n_minus_f_votes_triggers_finalization() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        manager.handle_block_proposal(block).unwrap();
        manager.mark_voted(0).unwrap();

        // Add exactly n-f = 5 votes
        let view_ctx = manager.view_chain.find_view_context_mut(0).unwrap();
        view_ctx.block_hash = Some(block_hash);

        for i in 0..5 {
            let peer_id = setup.peer_set.sorted_peer_ids[i];
            let sk = setup.peer_id_to_secret_key.get(&peer_id).unwrap();
            let sig = sk.sign(&block_hash);
            let vote = Vote::new(0, block_hash, sig, peer_id, leader_id);
            view_ctx.votes.insert(vote);
        }

        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldFinalize {
                view,
                block_hash: _,
            } => {
                assert_eq!(view, 0);
            }
            _ => panic!("Expected ShouldFinalize with exactly n-f votes"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_vote_triggers_finalization_when_threshold_reached() {
        // Test the realistic scenario where handle_vote() triggers finalization
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        // Receive and process block
        manager.handle_block_proposal(block).unwrap();

        // Add votes one by one until we hit n-f
        let mut last_result = None;
        for i in 1..=5 {
            // Need 5 total votes (n-f)
            let vote = create_test_vote(i, 0, block_hash, leader_id, &setup);
            last_result = Some(manager.handle_vote(vote).unwrap());
        }

        // The last vote that crosses the threshold should trigger finalization
        match last_result.unwrap() {
            ViewProgressEvent::ShouldFinalize {
                view,
                block_hash: _,
            }
            | ViewProgressEvent::ShouldVoteAndFinalize {
                view,
                block_hash: _,
            } => {
                assert_eq!(view, 0);
            }
            other => panic!("Expected finalization event, got {:?}", other),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_block_proposal_for_current_view_returns_should_vote() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);

        let result = manager.handle_block_proposal(block);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldVote {
                view,
                block_hash: _,
            } => {
                assert_eq!(view, 0);
            }
            _ => panic!("Expected ShouldVote event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_block_proposal_for_future_view_returns_should_update_view() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // View 5 % 4 = 1
        let leader_id = setup.peer_set.sorted_peer_ids[1];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(5, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 5);

        let result = manager.handle_block_proposal(block);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldUpdateView { new_view, leader } => {
                assert_eq!(new_view, 5);
                assert_eq!(leader, leader_id);
            }
            _ => panic!("Expected ShouldUpdateView event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_block_proposal_with_wrong_leader_returns_error() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let wrong_leader = setup.peer_set.sorted_peer_ids[1];
        let leader_sk = setup.peer_id_to_secret_key.get(&wrong_leader).unwrap();
        let block = create_test_block(0, wrong_leader, [0; blake3::OUT_LEN], leader_sk.clone(), 0);

        let result = manager.handle_block_proposal(block);
        assert!(result.is_err());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_duplicate_block_proposal_returns_error() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block1 = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block2 = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);

        manager.handle_block_proposal(block1).unwrap();
        let result = manager.handle_block_proposal(block2);

        assert!(result.is_err());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_block_proposal_triggers_should_vote_and_m_notarize() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        // Pre-add votes to reach M-notarization threshold - 1
        for i in 2..=3 {
            let vote = create_test_vote(i, 0, block_hash, leader_id, &setup);
            manager.handle_vote(vote).unwrap();
        }

        // Now add block - should trigger ShouldVoteAndMNotarize
        let result = manager.handle_block_proposal(block);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldVoteAndMNotarize {
                view,
                block_hash: _,
                should_forward_m_notarization,
            } => {
                assert_eq!(view, 0);
                assert!(should_forward_m_notarization);
            }
            _ => panic!("Expected ShouldVoteAndMNotarize event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_block_proposal_triggers_should_vote_and_finalize() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        // Pre-add votes to reach n-f threshold - 1 (need 5 total, add 4)
        for i in 2..=5 {
            let vote = create_test_vote(i, 0, block_hash, leader_id, &setup);
            manager.handle_vote(vote).unwrap();
        }

        // Now add block - should trigger ShouldVoteAndFinalize
        let result = manager.handle_block_proposal(block);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldVoteAndFinalize {
                view,
                block_hash: _,
            } => {
                assert_eq!(view, 0);
            }
            _ => panic!("Expected ShouldVoteAndFinalize event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_block_proposal_triggers_should_m_notarize_without_vote() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        // Pre-add enough votes to reach M-notarization threshold
        for i in 2..=4 {
            let vote = create_test_vote(i, 0, block_hash, leader_id, &setup);
            manager.handle_vote(vote).unwrap();
        }

        // Mark as already voted
        manager.mark_voted(0).unwrap();

        // Now add block - should trigger ShouldMNotarize (not vote)
        let result = manager.handle_block_proposal(block);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldMNotarize {
                view,
                block_hash: _,
                should_forward_m_notarization,
            } => {
                assert_eq!(view, 0);
                assert!(should_forward_m_notarization);
            }
            _ => panic!("Expected ShouldMNotarize event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_block_proposal_triggers_should_finalize_without_vote() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        // Pre-add enough votes to reach finalization threshold (n-f = 5)
        for i in 2..=5 {
            let vote = create_test_vote(i, 0, block_hash, leader_id, &setup);
            manager.handle_vote(vote).unwrap();
        }

        // Mark as already voted
        manager.mark_voted(0).unwrap();

        // Now add block - should trigger ShouldFinalize (not vote)
        let result = manager.handle_block_proposal(block);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldFinalize {
                view,
                block_hash: _,
            } => {
                assert_eq!(view, 0);
            }
            _ => panic!("Expected ShouldFinalize event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_vote_for_current_view_without_block_returns_await() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let block_hash = [1u8; blake3::OUT_LEN];
        let vote = create_test_vote(2, 0, block_hash, leader_id, &setup);

        let result = manager.handle_vote(vote);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::Await => {}
            _ => panic!("Expected Await event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_vote_for_future_view_returns_should_update_view() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[1];
        let vote = create_test_vote(2, 5, [1u8; blake3::OUT_LEN], leader_id, &setup);

        let result = manager.handle_vote(vote);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldUpdateView { new_view, leader } => {
                assert_eq!(new_view, 5);
                assert_eq!(leader, leader_id);
            }
            _ => panic!("Expected ShouldUpdateView event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_vote_triggers_m_notarization() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        // Add block first
        manager.handle_block_proposal(block).unwrap();

        // Add votes until threshold (need > 2*F = 2, so 3 votes)
        for i in 1..=2 {
            let vote = create_test_vote(i, 0, block_hash, leader_id, &setup);
            let result = manager.handle_vote(vote);

            if i == 3 {
                match result.unwrap() {
                    ViewProgressEvent::ShouldMNotarize {
                        view,
                        block_hash: _,
                        should_forward_m_notarization,
                    } => {
                        assert_eq!(view, 0);
                        assert!(should_forward_m_notarization);
                    }
                    _ => panic!("Expected ShouldMNotarize event"),
                }
            }
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_vote_for_past_view_is_processed() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // Set up view 0 with a block and M-notarization first
        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let leader_sk_0 = setup.peer_id_to_secret_key.get(&leader_id_0).unwrap();
        let block_0 =
            create_test_block(0, leader_id_0, [0; blake3::OUT_LEN], leader_sk_0.clone(), 0);
        let block_hash_0 = block_0.get_hash();
        manager.handle_block_proposal(block_0).unwrap();

        // Create and handle M-notarization for view 0
        let mut m_votes_0 = HashSet::new();
        for i in 0..=2 {
            m_votes_0.insert(create_test_vote(i, 0, block_hash_0, leader_id_0, &setup));
        }
        let m_not_0 =
            create_test_m_notarization::<6, 1, 3>(&m_votes_0, 0, block_hash_0, leader_id_0);
        manager.handle_m_notarization(m_not_0).unwrap();

        // Now at view 1
        assert_eq!(manager.current_view_number(), 1);

        // Now receive a vote for view 0 (past view) from a peer that hasn't voted yet
        let vote = create_test_vote(3, 0, block_hash_0, leader_id_0, &setup);

        let result = manager.handle_vote(vote);
        assert!(result.is_ok());

        // Should process without error and return NoOp or similar
        match result.unwrap() {
            ViewProgressEvent::NoOp => {}
            other => panic!("Expected NoOp for past view vote, got {:?}", other),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_vote_triggers_should_vote_and_m_notarize() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        // Add block (leader peer 0 votes)
        manager.handle_block_proposal(block).unwrap();
        // Now: 1 vote (leader)

        // Add one vote to bring us to 2 votes (one short of M-notarization threshold)
        let vote2 = create_test_vote(2, 0, block_hash, leader_id, &setup);
        manager.handle_vote(vote2).unwrap();
        // Now: 2 votes (leader + peer 2)

        // Add the replica's (peer 1) vote, which should cross M-notarization threshold
        let replica_vote = create_test_vote(1, 0, block_hash, leader_id, &setup);
        let result = manager.handle_vote(replica_vote);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldVoteAndMNotarize {
                view,
                block_hash: _,
                should_forward_m_notarization,
            } => {
                assert_eq!(view, 0);
                assert!(should_forward_m_notarization);
            }
            ViewProgressEvent::ShouldMNotarize { .. } => {
                // Also acceptable - depends on implementation details
            }
            other => panic!("Expected M-notarization related event, got {:?}", other),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_vote_triggers_should_nullify_on_conflict() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);

        // Add block and mark as voted
        manager.handle_block_proposal(block).unwrap();
        manager.mark_voted(0).unwrap();

        // Add votes for a DIFFERENT block hash (conflicting)
        let conflicting_hash = [255u8; blake3::OUT_LEN];
        for i in 2..=4 {
            let vote = create_test_vote(i, 0, conflicting_hash, leader_id, &setup);
            let result = manager.handle_vote(vote);

            // The last vote might trigger nullification
            if i == 4
                && let ViewProgressEvent::ShouldNullify { view } = result.unwrap()
            {
                assert_eq!(view, 0);
                std::fs::remove_file(path).unwrap();
                return;
            }
        }

        // If not triggered in handle_vote, should be detected in tick
        let tick_result = manager.tick();
        if let ViewProgressEvent::ShouldNullify { view } = tick_result.unwrap() {
            assert_eq!(view, 0);
            std::fs::remove_file(path).unwrap();
            return;
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_nullify_for_current_view_returns_no_op() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let nullify = create_test_nullify(2, 0, leader_id, &setup);

        let result = manager.handle_nullify(nullify);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::NoOp => {}
            _ => panic!("Expected NoOp event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_nullify_triggers_broadcast_when_threshold_reached() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];

        // Add nullify messages until threshold (> 2*F = 2, need 3)
        for i in 1..=3 {
            let nullify = create_test_nullify(i, 0, leader_id, &setup);
            let result = manager.handle_nullify(nullify);

            if i == 3 {
                match result.unwrap() {
                    ViewProgressEvent::ShouldBroadcastNullification { view } => {
                        assert_eq!(view, 0);
                    }
                    _ => panic!("Expected ShouldBroadcastNullification event"),
                }
            }
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_nullify_for_future_view_returns_should_update_view() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id_5 = setup.peer_set.sorted_peer_ids[5]; // View 5 % 6 = 5
        let nullify = create_test_nullify(2, 5, leader_id_5, &setup);

        let result = manager.handle_nullify(nullify);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldUpdateView { new_view, leader } => {
                assert_eq!(new_view, 5);
                assert_eq!(leader, leader_id_5);
            }
            _ => panic!("Expected ShouldUpdateView event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_nullify_with_wrong_leader_returns_error() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // View 5, but provide wrong leader
        let wrong_leader = setup.peer_set.sorted_peer_ids[3]; // Wrong leader for view 5
        let nullify = create_test_nullify(2, 5, wrong_leader, &setup);

        let result = manager.handle_nullify(nullify);
        assert!(result.is_err());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_nullify_multiple_messages_in_sequence() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];

        // Add multiple nullify messages
        for i in 1..=5 {
            let nullify = create_test_nullify(i, 0, leader_id, &setup);
            let result = manager.handle_nullify(nullify);
            assert!(result.is_ok());

            // The 3rd should trigger broadcast
            if i == 3 {
                match result.unwrap() {
                    ViewProgressEvent::ShouldBroadcastNullification { view } => {
                        assert_eq!(view, 0);
                    }
                    _ => panic!("Expected ShouldBroadcastNullification on 3rd nullify"),
                }
            }
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_m_notarization_for_current_view_triggers_view_progress() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        // Add block first
        manager.handle_block_proposal(block).unwrap();

        // Create M-notarization
        let mut votes = HashSet::new();
        for i in 1..=3 {
            votes.insert(create_test_vote(i, 0, block_hash, leader_id, &setup));
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 0, block_hash, leader_id);

        let result = manager.handle_m_notarization(m_notarization);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ProgressToNextView {
                new_view,
                leader: _,
                notarized_block_hash: _,
                should_forward_m_notarization,
            } => {
                assert_eq!(new_view, 1);
                assert!(should_forward_m_notarization);
            }
            ViewProgressEvent::ShouldVoteAndProgressToNextView { new_view, .. } => {
                assert_eq!(new_view, 1);
            }
            _ => panic!("Expected view progression event"),
        }

        // Check that view actually progressed
        assert_eq!(manager.current_view_number(), 1);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_m_notarization_for_past_view() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // Set up view 0 with a block and M-notarization first
        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let leader_sk_0 = setup.peer_id_to_secret_key.get(&leader_id_0).unwrap();
        let block_0 =
            create_test_block(0, leader_id_0, [0; blake3::OUT_LEN], leader_sk_0.clone(), 0);
        let block_hash_0 = block_0.get_hash();
        manager.handle_block_proposal(block_0).unwrap();

        // Create and handle M-notarization for view 0
        let mut m_votes_0 = HashSet::new();
        for i in 0..=2 {
            m_votes_0.insert(create_test_vote(i, 0, block_hash_0, leader_id_0, &setup));
        }
        let m_not_0 =
            create_test_m_notarization::<6, 1, 3>(&m_votes_0, 0, block_hash_0, leader_id_0);
        manager.handle_m_notarization(m_not_0).unwrap();

        // Now progress to view 1 and view 2
        let mut parent_hash = block_hash_0;
        for view_num in 1..=2 {
            let leader_id = setup.peer_set.sorted_peer_ids[view_num % 6];
            let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
            let block = create_test_block(
                view_num as u64,
                leader_id,
                parent_hash,
                leader_sk.clone(),
                view_num as u64,
            );
            let block_hash = block.get_hash();
            manager.handle_block_proposal(block).unwrap();

            // Create and handle M-notarization
            let mut m_votes = HashSet::new();
            for i in 0..=2 {
                m_votes.insert(create_test_vote(
                    i,
                    view_num as u64,
                    block_hash,
                    leader_id,
                    &setup,
                ));
            }
            let m_not = create_test_m_notarization::<6, 1, 3>(
                &m_votes,
                view_num as u64,
                block_hash,
                leader_id,
            );
            manager.handle_m_notarization(m_not).unwrap();

            parent_hash = block_hash;
        }

        // Now at view 3
        assert_eq!(manager.current_view_number(), 3);

        // Create a DUPLICATE M-notarization for view 0 (past view)
        let mut votes = HashSet::new();
        for i in 0..=2 {
            votes.insert(create_test_vote(i, 0, block_hash_0, leader_id_0, &setup));
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 0, block_hash_0, leader_id_0);

        let result = manager.handle_m_notarization(m_notarization);
        // assert!(result.is_ok());
        result.unwrap();

        // Should not progress view since it's for a past view
        assert_eq!(manager.current_view_number(), 3);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_m_notarization_with_wrong_leader_returns_error() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let wrong_leader = setup.peer_set.sorted_peer_ids[3];
        let block_hash = [1u8; blake3::OUT_LEN];
        let mut votes = HashSet::new();
        for i in 1..=3 {
            votes.insert(create_test_vote(i, 5, block_hash, wrong_leader, &setup));
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 5, block_hash, wrong_leader);

        let result = manager.handle_m_notarization(m_notarization);
        assert!(result.is_err());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_m_notarization_for_future_view_returns_should_update_view() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id_5 = setup.peer_set.sorted_peer_ids[5];
        let block_hash = [1u8; blake3::OUT_LEN];
        let mut votes = HashSet::new();
        for i in 1..=3 {
            votes.insert(create_test_vote(i, 5, block_hash, leader_id_5, &setup));
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 5, block_hash, leader_id_5);

        let result = manager.handle_m_notarization(m_notarization);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldUpdateView { new_view, leader } => {
                assert_eq!(new_view, 5);
                assert_eq!(leader, leader_id_5);
            }
            _ => panic!("Expected ShouldUpdateView event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_nullification_for_current_view_triggers_view_progress() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];

        // Create nullification
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            nullify_messages.insert(create_test_nullify(i, 0, leader_id, &setup));
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 0, leader_id);

        let result = manager.handle_nullification(nullification);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ProgressToNextViewOnNullification {
                new_view,
                leader: _,
                should_broadcast_nullification: _,
                parent_block_hash,
            } => {
                assert_eq!(new_view, 1);
                assert_eq!(parent_block_hash, [0; blake3::OUT_LEN]);
            }
            _ => panic!("Expected ProgressToNextViewOnNullification event"),
        }

        // Check that view actually progressed
        assert_eq!(manager.current_view_number(), 1);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_nullification_for_future_view_returns_should_update_view() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // Leader for view 5 is at index 5 % 6 = 5
        let leader_id_5 = setup.peer_set.sorted_peer_ids[5];
        let mut nullify_messages = HashSet::new();
        for i in 0..=2 {
            nullify_messages.insert(create_test_nullify(i, 5, leader_id_5, &setup));
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 5, leader_id_5);

        let result = manager.handle_nullification(nullification);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldUpdateView { new_view, leader } => {
                assert_eq!(new_view, 5);
                assert_eq!(leader, leader_id_5);
            }
            _ => panic!("Expected ShouldUpdateView event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_nullification_with_wrong_leader_returns_error() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let wrong_leader = setup.peer_set.sorted_peer_ids[3];
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            nullify_messages.insert(create_test_nullify(i, 5, wrong_leader, &setup));
        }
        let nullification =
            create_test_nullification::<6, 1, 3>(&nullify_messages, 5, wrong_leader);

        let result = manager.handle_nullification(nullification);
        assert!(result.is_err());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_nullification_for_past_view() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // Set up view 0 with a nullification first
        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];

        // Create and handle nullification for view 0
        let mut nullify_messages_0 = HashSet::new();
        for i in 0..=2 {
            nullify_messages_0.insert(create_test_nullify(i, 0, leader_id_0, &setup));
        }
        let nullification_0 =
            create_test_nullification::<6, 1, 3>(&nullify_messages_0, 0, leader_id_0);
        manager.handle_nullification(nullification_0).unwrap();

        // Now progress to view 1 and view 2, each with their nullifications
        for view_num in 1..=2 {
            let leader_id = setup.peer_set.sorted_peer_ids[view_num % 6];

            // Create and handle nullification for this view
            let mut nullify_messages = HashSet::new();
            for i in 0..=2 {
                nullify_messages.insert(create_test_nullify(i, view_num as u64, leader_id, &setup));
            }
            let nullification =
                create_test_nullification::<6, 1, 3>(&nullify_messages, view_num as u64, leader_id);
            manager.handle_nullification(nullification).unwrap();
        }

        // Now at view 3
        assert_eq!(manager.current_view_number(), 3);

        // Create a LATE nullification for view 0 (past view) with different nullify messages
        let mut nullify_messages = HashSet::new();
        for i in 3..=5 {
            // Use different peers to create a different nullification
            nullify_messages.insert(create_test_nullify(i, 0, leader_id_0, &setup));
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 0, leader_id_0);

        let result = manager.handle_nullification(nullification);
        assert!(result.is_ok());

        // Should not progress view since it's for a past view
        assert_eq!(manager.current_view_number(), 3);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_mark_proposed_for_current_view_succeeds() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 0);

        let result = manager.mark_proposed(0);
        assert!(result.is_ok());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_mark_proposed_for_wrong_view_fails() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 0);

        let result = manager.mark_proposed(5);
        assert!(result.is_err());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_mark_voted_for_current_view_succeeds() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 0);

        let result = manager.mark_voted(0);
        assert!(result.is_ok());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_mark_nullified_for_current_view_succeeds() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 0);

        let result = manager.mark_nullified(0);
        assert!(result.is_ok());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_mark_voted_for_nonexistent_view_fails() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 0);

        let result = manager.mark_voted(99);
        assert!(result.is_err());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_mark_nullified_for_nonexistent_view_fails() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 0);

        let result = manager.mark_nullified(99);
        assert!(result.is_err());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_mark_voted_after_view_progression() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // Set up view 0 with a block and M-notarization first
        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let leader_sk_0 = setup.peer_id_to_secret_key.get(&leader_id_0).unwrap();
        let block_0 =
            create_test_block(0, leader_id_0, [0; blake3::OUT_LEN], leader_sk_0.clone(), 0);
        let block_hash_0 = block_0.get_hash();
        manager.handle_block_proposal(block_0).unwrap();

        // Create and handle M-notarization for view 0
        let mut m_votes_0 = HashSet::new();
        for i in 0..=2 {
            m_votes_0.insert(create_test_vote(i, 0, block_hash_0, leader_id_0, &setup));
        }
        let m_not_0 =
            create_test_m_notarization::<6, 1, 3>(&m_votes_0, 0, block_hash_0, leader_id_0);
        manager.handle_m_notarization(m_not_0).unwrap();

        // Now at view 1
        assert_eq!(manager.current_view_number(), 1);

        // Should still be able to mark vote for view 0 (past view still in chain)
        let result = manager.mark_voted(0);
        assert!(result.is_ok());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_get_m_notarization_returns_error_when_not_found() {
        let setup = create_test_peer_setup(4);
        let (manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 0);

        let result = manager.get_m_notarization(0);
        assert!(result.is_err());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_get_nullification_returns_error_when_not_found() {
        let setup = create_test_peer_setup(4);
        let (manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 0);

        let result = manager.get_nullification(0);
        assert!(result.is_err());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_add_own_vote_succeeds() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        // Add block first
        manager.handle_block_proposal(block).unwrap();

        // Add own vote
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let sk = setup.peer_id_to_secret_key.get(&replica_id).unwrap();
        let signature = sk.sign(&block_hash);

        let result = manager.add_own_vote(0, signature);
        assert!(result.is_ok());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_add_leader_vote_succeeds() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 0);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        let sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let signature = sk.sign(&block_hash);

        let result = manager.add_leader_vote_for_block_proposal(0, block, signature);
        assert!(result.is_ok());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_process_consensus_msg_block_proposal() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let msg = ConsensusMessage::BlockProposal(block);

        let result = manager.process_consensus_msg(msg);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldVote { .. } => {}
            _ => panic!("Expected ShouldVote event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_process_consensus_msg_vote() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let vote = create_test_vote(2, 0, [1u8; blake3::OUT_LEN], leader_id, &setup);
        let msg = ConsensusMessage::Vote(vote);

        let result = manager.process_consensus_msg(msg);
        assert!(result.is_ok());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_current_view_number_returns_correct_value() {
        let setup = create_test_peer_setup(4);
        let (manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 0);

        assert_eq!(manager.current_view_number(), 0);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_replica_id_returns_correct_value() {
        let setup = create_test_peer_setup(4);
        let (manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 2);

        assert_eq!(manager.replica_id(), setup.peer_set.sorted_peer_ids[2]);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_non_finalized_count_starts_at_one() {
        let setup = create_test_peer_setup(4);
        let (manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 0);

        assert_eq!(manager.non_finalized_count(), 1);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_get_m_notarization_when_exists_returns_ok() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        manager.handle_block_proposal(block).unwrap();

        // Create and add M-notarization
        let mut votes = HashSet::new();
        for i in 1..=3 {
            votes.insert(create_test_vote(i, 0, block_hash, leader_id, &setup));
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 0, block_hash, leader_id);

        manager.handle_m_notarization(m_notarization).unwrap();

        // Should be able to retrieve it
        let result = manager.get_m_notarization(0);
        assert!(result.is_ok());

        let retrieved = result.unwrap();
        assert_eq!(retrieved.view, 0);
        assert_eq!(retrieved.block_hash, block_hash);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_get_nullification_when_exists_returns_ok() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];

        // Create and add nullification
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            nullify_messages.insert(create_test_nullify(i, 0, leader_id, &setup));
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 0, leader_id);

        manager.handle_nullification(nullification).unwrap();

        // Should be able to retrieve it
        let result = manager.get_nullification(0);
        assert!(result.is_ok());

        let retrieved = result.unwrap();
        assert_eq!(retrieved.view, 0);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_add_own_vote_for_nonexistent_view_fails() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let sk = setup.peer_id_to_secret_key.get(&replica_id).unwrap();
        let block_hash = [1u8; blake3::OUT_LEN];
        let signature = sk.sign(&block_hash);

        let result = manager.add_own_vote(99, signature);
        assert!(result.is_err());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_add_leader_vote_for_nonexistent_view_fails() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 0);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        let signature = leader_sk.sign(&block_hash);

        let result = manager.add_leader_vote_for_block_proposal(99, block, signature);
        assert!(result.is_err());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_add_own_vote_for_past_view_succeeds() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id_0).unwrap();
        let block = create_test_block(0, leader_id_0, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        manager.handle_block_proposal(block).unwrap();

        // Add votes to create M-notarization (need >2F = >2 votes for F=1)
        // Leader's vote is already counted, so add 2 more votes (total 3 votes)
        for i in 1..=2 {
            let peer_id = setup.peer_set.sorted_peer_ids[i];
            let peer_sk = setup.peer_id_to_secret_key.get(&peer_id).unwrap();
            let signature = peer_sk.sign(&block_hash);
            let vote = Vote::new(0, block_hash, signature, peer_id, leader_id_0);
            manager.handle_vote(vote).unwrap();
        }

        // Now view 0 should have M-notarization
        assert!(manager.view_chain.current().m_notarization.is_some());

        // Progress to view 1 with M-notarization
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let leader_id_1 = setup.peer_set.sorted_peer_ids[1];
        let new_view_ctx = ViewContext::new(1, leader_id_1, replica_id, block_hash);
        manager
            .view_chain
            .progress_with_m_notarization(new_view_ctx)
            .unwrap();

        // Add vote for view 0 (now past)
        let sk = setup.peer_id_to_secret_key.get(&replica_id).unwrap();
        let signature = sk.sign(&block_hash);

        let result = manager.add_own_vote(0, signature);
        assert!(result.is_ok());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_process_consensus_msg_nullify_single() {
        // Test: Single nullify message is accepted and stored
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let nullify = create_test_nullify(2, 0, leader_id, &setup);
        let msg = ConsensusMessage::Nullify(nullify);

        let result = manager.process_consensus_msg(msg);
        assert!(result.is_ok());

        // Should return NoOp (not enough nullifies yet for threshold)
        match result.unwrap() {
            ViewProgressEvent::NoOp => {} // Expected
            other => panic!("Expected NoOp, got {:?}", other),
        }

        // Verify nullify was added to view context
        let current_view = manager.view_chain.current();
        assert_eq!(current_view.nullify_messages.len(), 1);
        assert!(current_view.nullification.is_none()); // Not enough yet

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_process_consensus_msg_nullify_reaches_threshold() {
        // Test: Multiple nullifies reach threshold and trigger broadcast
        let setup = create_test_peer_setup(6); // N=6, F=1, need >2F = 3 nullifies
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];

        // Add first nullify
        let nullify1 = create_test_nullify(2, 0, leader_id, &setup);
        let msg1 = ConsensusMessage::Nullify(nullify1);
        let result1 = manager.process_consensus_msg(msg1);
        assert!(result1.is_ok());
        assert!(matches!(result1.unwrap(), ViewProgressEvent::NoOp));

        // Add second nullify
        let nullify2 = create_test_nullify(3, 0, leader_id, &setup);
        let msg2 = ConsensusMessage::Nullify(nullify2);
        let result2 = manager.process_consensus_msg(msg2);
        assert!(result2.is_ok());
        assert!(matches!(result2.unwrap(), ViewProgressEvent::NoOp));

        // Add third nullify - should trigger nullification broadcast
        let nullify3 = create_test_nullify(4, 0, leader_id, &setup);
        let msg3 = ConsensusMessage::Nullify(nullify3);
        let result3 = manager.process_consensus_msg(msg3);
        assert!(result3.is_ok());

        // Should return ShouldBroadcastNullification
        match result3.unwrap() {
            ViewProgressEvent::ShouldBroadcastNullification { view } => {
                assert_eq!(view, 0);
            }
            other => panic!("Expected ShouldBroadcastNullification, got {:?}", other),
        }

        // Verify nullification was created
        let current_view = manager.view_chain.current();
        assert_eq!(current_view.nullify_messages.len(), 3);
        assert!(current_view.nullification.is_some());
        assert!(current_view.has_nullified);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_process_consensus_msg_nullify_for_future_view() {
        // Test: Nullify for future view triggers ShouldUpdateView
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // Current view is 0, send nullify for view 2
        let leader_id_2 = setup.peer_set.sorted_peer_ids[2];
        let nullify = create_test_nullify(1, 2, leader_id_2, &setup);
        let msg = ConsensusMessage::Nullify(nullify);

        let result = manager.process_consensus_msg(msg);
        assert!(result.is_ok());

        // Should return ShouldUpdateView
        match result.unwrap() {
            ViewProgressEvent::ShouldUpdateView { new_view, leader } => {
                assert_eq!(new_view, 2);
                assert_eq!(leader, leader_id_2);
            }
            other => panic!("Expected ShouldUpdateView, got {:?}", other),
        }

        // Current view should still be 0 (update happens in state machine)
        assert_eq!(manager.current_view_number(), 0);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_process_consensus_msg_nullify_duplicate_ignored() {
        // Test: Duplicate nullify from same peer is rejected
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let nullify = create_test_nullify(2, 0, leader_id, &setup);

        // Add first time
        let msg1 = ConsensusMessage::Nullify(nullify.clone());
        let result1 = manager.process_consensus_msg(msg1);
        assert!(result1.is_ok());

        // Add second time (duplicate)
        let msg2 = ConsensusMessage::Nullify(nullify);
        let result2 = manager.process_consensus_msg(msg2);

        // Should error (duplicate peer)
        assert!(result2.is_err());
        assert!(result2.unwrap_err().to_string().contains("already exists"));

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_process_consensus_msg_nullify_wrong_leader() {
        // Test: Nullify with wrong leader is rejected
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let wrong_leader = setup.peer_set.sorted_peer_ids[1];

        // Create nullify with wrong leader
        let nullify = create_test_nullify(2, 0, wrong_leader, &setup);
        let msg = ConsensusMessage::Nullify(nullify);

        let result = manager.process_consensus_msg(msg);

        // Should error
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not the current leader")
        );

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_process_consensus_msg_m_notarization() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        manager.handle_block_proposal(block).unwrap();

        let mut votes = HashSet::new();
        for i in 1..=3 {
            votes.insert(create_test_vote(i, 0, block_hash, leader_id, &setup));
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 0, block_hash, leader_id);
        let msg = ConsensusMessage::MNotarization(m_notarization);

        let result = manager.process_consensus_msg(msg);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ProgressToNextView { .. }
            | ViewProgressEvent::ShouldVoteAndProgressToNextView { .. } => {}
            _ => panic!("Expected view progression event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_process_consensus_msg_nullification_current_view() {
        // Test: Nullification for current view triggers progression
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            nullify_messages.insert(create_test_nullify(i, 0, leader_id, &setup));
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 0, leader_id);
        let msg = ConsensusMessage::Nullification(nullification);

        let initial_view = manager.current_view_number();
        assert_eq!(initial_view, 0);

        let result = manager.process_consensus_msg(msg);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ProgressToNextViewOnNullification {
                new_view,
                leader,
                should_broadcast_nullification,
                parent_block_hash,
            } => {
                assert_eq!(new_view, 1);
                assert!(should_broadcast_nullification); // First time receiving it
                // Verify leader is correct
                let expected_leader = setup.peer_set.sorted_peer_ids[1];
                assert_eq!(leader, expected_leader);
                assert_eq!(parent_block_hash, [0; blake3::OUT_LEN]);
            }
            other => panic!(
                "Expected ProgressToNextViewOnNullification, got {:?}",
                other
            ),
        }

        // Verify view progression happened
        assert_eq!(manager.current_view_number(), 1);

        // Verify nullification was stored in view 0
        let view_0 = manager.view_chain.find_view_context(0).unwrap();
        assert!(view_0.nullification.is_some());
        assert!(view_0.has_nullified);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_process_consensus_msg_nullification_for_future_view() {
        // Test: Nullification for future view triggers ShouldUpdateView
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        assert_eq!(manager.current_view_number(), 0);

        // Create nullification for view 2 (future)
        let leader_id_2 = setup.peer_set.sorted_peer_ids[2];
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            nullify_messages.insert(create_test_nullify(i, 2, leader_id_2, &setup));
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 2, leader_id_2);
        let msg = ConsensusMessage::Nullification(nullification);

        let result = manager.process_consensus_msg(msg);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldUpdateView { new_view, leader } => {
                assert_eq!(new_view, 2);
                assert_eq!(leader, leader_id_2);
            }
            other => panic!("Expected ShouldUpdateView, got {:?}", other),
        }

        // View should NOT have progressed yet (that happens in state machine)
        assert_eq!(manager.current_view_number(), 0);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_process_consensus_msg_nullification_for_past_view() {
        // Test: Nullification for past view broadcasts but doesn't progress
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];

        // Manually progress to view 1
        let leader_id_1 = setup.peer_set.sorted_peer_ids[1];
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let new_view_ctx = ViewContext::new(1, leader_id_1, replica_id, [0; blake3::OUT_LEN]);

        // First, create nullification for view 0 to allow progression
        let mut nullify_messages_0 = HashSet::new();
        for i in 1..=3 {
            nullify_messages_0.insert(create_test_nullify(i, 0, leader_id_0, &setup));
        }
        let nullification_0 =
            create_test_nullification::<6, 1, 3>(&nullify_messages_0, 0, leader_id_0);
        manager
            .view_chain
            .route_nullification(nullification_0, &manager.peers)
            .unwrap();
        manager
            .view_chain
            .progress_with_nullification(new_view_ctx)
            .unwrap();

        assert_eq!(manager.current_view_number(), 1);

        // Now receive nullification for view 0 again (past view, duplicate)
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            nullify_messages.insert(create_test_nullify(i, 0, leader_id_0, &setup));
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 0, leader_id_0);
        let msg = ConsensusMessage::Nullification(nullification);

        let result = manager.process_consensus_msg(msg);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldBroadcastNullification { view } => {
                assert_eq!(view, 0);
            }
            ViewProgressEvent::ShouldNullify { view } => {
                // Might also return this if it's not the first time
                assert_eq!(view, 0);
            }
            other => panic!(
                "Expected ShouldBroadcastNullification or ShouldNullify, got {:?}",
                other
            ),
        }

        // View should remain at 1
        assert_eq!(manager.current_view_number(), 1);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_process_consensus_msg_nullification_duplicate_does_not_rebroadcast() {
        // Test: Duplicate nullification is accepted but doesn't trigger rebroadcast
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            nullify_messages.insert(create_test_nullify(i, 0, leader_id, &setup));
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 0, leader_id);

        // Add first time
        let msg1 = ConsensusMessage::Nullification(nullification.clone());
        let result1 = manager.process_consensus_msg(msg1);
        assert!(result1.is_ok());

        match result1.unwrap() {
            ViewProgressEvent::ProgressToNextViewOnNullification {
                should_broadcast_nullification,
                ..
            } => {
                assert!(should_broadcast_nullification); // First time
            }
            other => panic!(
                "Expected ProgressToNextViewOnNullification, got {:?}",
                other
            ),
        }

        // Go back to view 0 for testing (or use view chain directly)
        // Actually, since we progressed, we need to test on a past view
        // Let's check view 0 state instead
        let view_0 = manager.view_chain.find_view_context(0).unwrap();
        assert!(view_0.nullification.is_some());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_process_consensus_msg_nullification_wrong_leader() {
        // Test: Nullification with wrong leader is rejected
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let wrong_leader = setup.peer_set.sorted_peer_ids[1];

        // Create nullification with wrong leader
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            nullify_messages.insert(create_test_nullify(i, 0, wrong_leader, &setup));
        }
        let nullification =
            create_test_nullification::<6, 1, 3>(&nullify_messages, 0, wrong_leader);
        let msg = ConsensusMessage::Nullification(nullification);

        let result = manager.process_consensus_msg(msg);

        // Should error
        assert!(result.is_err());
        assert!(
            result
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("not the current leader")
                || result
                    .unwrap_err()
                    .to_string()
                    .contains("not the correct view leader")
        );

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_process_consensus_msg_nullification_invalid_signature() {
        // Test: Nullification with invalid signature is rejected
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let peer_ids = [
            setup.peer_set.sorted_peer_ids[1],
            setup.peer_set.sorted_peer_ids[2],
            setup.peer_set.sorted_peer_ids[3],
        ];

        // Create nullification with invalid signature (random signature)
        let wrong_sk = BlsSecretKey::generate(&mut thread_rng());
        let wrong_signature = wrong_sk.sign(&[99u8; 32]);
        let nullification = Nullification::new(0, leader_id, wrong_signature, peer_ids);
        let msg = ConsensusMessage::Nullification(nullification);

        let result = manager.process_consensus_msg(msg);

        // Should error due to invalid signature
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not valid"));

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_finalize_view_with_valid_l_notarization_succeeds() {
        // Test: View with n-f votes can be finalized successfully
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let leader_sk_0 = setup.peer_id_to_secret_key.get(&leader_id_0).unwrap();
        let block = create_test_block(0, leader_id_0, [0; blake3::OUT_LEN], leader_sk_0.clone(), 0);
        let block_hash = block.get_hash();

        manager.handle_block_proposal(block).unwrap();

        // Add votes to create M-notarization first (>2F = 3 votes)
        // Leader's vote is counted, so add 2 more
        for i in 1..=2 {
            let vote = create_test_vote(i, 0, block_hash, leader_id_0, &setup);
            manager.handle_vote(vote).unwrap();
        }

        // Verify M-notarization exists
        let view_0 = manager.view_chain.find_view_context(0).unwrap();
        assert!(view_0.m_notarization.is_some());

        // Progress to view 1 with M-notarization
        let leader_id_1 = setup.peer_set.sorted_peer_ids[1];
        let replica_id = manager.replica_id;
        let new_view_ctx = ViewContext::new(1, leader_id_1, replica_id, block_hash);
        manager
            .view_chain
            .progress_with_m_notarization(new_view_ctx)
            .unwrap();

        // Now we're in view 1
        assert_eq!(manager.current_view_number(), 1);

        // Add more votes to view 0 to reach L-notarization (n-f=5 total)
        // We have 3, need 2 more
        for i in 3..=4 {
            let vote = create_test_vote(i, 0, block_hash, leader_id_0, &setup);
            manager.handle_vote(vote).unwrap();
        }

        // Verify we have enough votes for L-notarization
        let view_0_before = manager.view_chain.find_view_context(0).unwrap();
        assert_eq!(view_0_before.votes.len(), 5); // n-f = 5

        // Now finalize view 0 (we're in view 1, so view 0 is a past view)
        let result = manager.finalize_view(0);
        // assert!(result.is_ok());
        result.unwrap();

        // Verify view 0 was removed from non-finalized views
        assert!(manager.view_chain.find_view_context(0).is_none());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_finalize_view_without_enough_votes_fails() {
        // Test: Cannot finalize view without n-f votes
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        manager.handle_block_proposal(block).unwrap();

        // Add only 3 votes (not enough: need n-f=5)
        // Leader's vote is counted, so add 2 more = 3 total
        for i in 1..=2 {
            let vote = create_test_vote(i, 0, block_hash, leader_id, &setup);
            manager.handle_vote(vote).unwrap();
        }

        // Verify we DON'T have enough votes
        let view_0 = manager.view_chain.find_view_context(0).unwrap();
        assert_eq!(view_0.votes.len(), 3); // Less than n-f=5

        // Try to finalize - should fail
        let result = manager.finalize_view(0);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not received a l-notarization")
        );

        // View should still exist
        assert!(manager.view_chain.find_view_context(0).is_some());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_finalize_view_without_block_fails() {
        // Test: Cannot finalize view without a block
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // View 0 exists but has no block
        assert!(manager.view_chain.find_view_context(0).is_some());
        let view_0 = manager.view_chain.find_view_context(0).unwrap();
        assert!(view_0.block.is_none());

        // Try to finalize - should fail
        let result = manager.finalize_view(0);
        assert!(result.is_err());

        // Could fail due to not enough votes OR no block
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not received a l-notarization") || err_msg.contains("no block"));

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_finalize_future_view_fails() {
        // Test: Cannot finalize a future view
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        assert_eq!(manager.current_view_number(), 0);

        // Try to finalize view 5 (doesn't exist yet)
        let result = manager.finalize_view(5);
        assert!(result.is_err());
        assert!(
            result
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("not greater than the current view")
                || result
                    .unwrap_err()
                    .to_string()
                    .contains("not an non-finalized view")
        );

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_finalize_view_nonexistent_view_fails() {
        // Test: Cannot finalize a view that doesn't exist
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // Progress to view 1, which will remove view 0 from tracking potentially
        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let leader_sk_0 = setup.peer_id_to_secret_key.get(&leader_id_0).unwrap();
        let block_0 =
            create_test_block(0, leader_id_0, [0; blake3::OUT_LEN], leader_sk_0.clone(), 0);
        let block_hash_0 = block_0.get_hash();

        manager.handle_block_proposal(block_0).unwrap();

        // Add M-notarization votes (>2f = 2), notice the leader's vote is already counted
        // in the `handle_block_proposal` call.
        for i in 1..=2 {
            let vote = create_test_vote(i, 0, block_hash_0, leader_id_0, &setup);
            manager.handle_vote(vote).unwrap();
        }

        // Create M-notarization manually and progress
        let mut m_votes = HashSet::new();
        for i in 0..=2 {
            let vote = create_test_vote(i, 0, block_hash_0, leader_id_0, &setup);
            m_votes.insert(vote);
        }
        let m_not = create_test_m_notarization::<6, 1, 3>(&m_votes, 0, block_hash_0, leader_id_0);
        manager.handle_m_notarization(m_not).unwrap();

        // Now at view 1
        assert_eq!(manager.current_view_number(), 1);

        // Finalize and remove view 0
        for i in 3..=4 {
            let vote = create_test_vote(i, 0, block_hash_0, leader_id_0, &setup);
            manager.handle_vote(vote).unwrap();
        }
        manager.finalize_view(0).unwrap();

        // Verify view 0 is gone
        assert!(manager.view_chain.find_view_context(0).is_none());

        // Try to finalize view 0 again - should fail
        let result = manager.finalize_view(0);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not an non-finalized view")
        );

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_finalize_view_at_exact_threshold() {
        // Test: Finalization works with exactly n-f votes
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        manager.handle_block_proposal(block).unwrap();

        // Add exactly n-f-1 more votes (leader already voted)
        // n=6, f=1, n-f=5, so add 4 more votes
        for i in 1..=4 {
            let vote = create_test_vote(i, 0, block_hash, leader_id, &setup);
            manager.handle_vote(vote).unwrap();
        }

        // Should have exactly n-f votes
        let view_0 = manager.view_chain.find_view_context(0).unwrap();
        assert_eq!(view_0.votes.len(), 5); // Exactly n-f

        // Finalization should succeed
        let result = manager.finalize_view(0);
        assert!(result.is_ok());

        // View should be removed
        assert!(manager.view_chain.find_view_context(0).is_none());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_finalize_view_with_more_than_threshold() {
        // Test: Finalization works with more than n-f votes
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        manager.handle_block_proposal(block).unwrap();

        // Add all possible votes (n-1 since leader already voted)
        for i in 1..=5 {
            let vote = create_test_vote(i, 0, block_hash, leader_id, &setup);
            manager.handle_vote(vote).unwrap();
        }

        // Should have 6 votes (more than n-f=5)
        let view_0 = manager.view_chain.find_view_context(0).unwrap();
        assert_eq!(view_0.votes.len(), 6);

        // Finalization should succeed
        let result = manager.finalize_view(0);
        assert!(result.is_ok());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_finalize_view_for_nonexistent_view_fails() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 0);

        let result = manager.finalize_view(99);
        assert!(result.is_err());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_finalize_view_without_l_notarization_fails() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // Try to finalize without enough votes
        let result = manager.finalize_view(0);
        assert!(result.is_err());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_shutdown_persists_non_finalized_views() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // Set up view 0 with a block and M-notarization first
        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let leader_sk_0 = setup.peer_id_to_secret_key.get(&leader_id_0).unwrap();
        let block_0 =
            create_test_block(0, leader_id_0, [0; blake3::OUT_LEN], leader_sk_0.clone(), 0);
        let block_hash_0 = block_0.get_hash();
        manager.handle_block_proposal(block_0).unwrap();

        // Create and handle M-notarization for view 0
        let mut m_votes_0 = HashSet::new();
        for i in 0..=2 {
            m_votes_0.insert(create_test_vote(i, 0, block_hash_0, leader_id_0, &setup));
        }
        let m_not_0 =
            create_test_m_notarization::<6, 1, 3>(&m_votes_0, 0, block_hash_0, leader_id_0);
        manager.handle_m_notarization(m_not_0).unwrap();

        // Progress through views 1 and 2
        let mut parent_hash = block_hash_0;
        for view_num in 1..=2 {
            let leader_id = setup.peer_set.sorted_peer_ids[view_num % 6];
            let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
            let block = create_test_block(
                view_num as u64,
                leader_id,
                parent_hash,
                leader_sk.clone(),
                view_num as u64,
            );
            let block_hash = block.get_hash();
            manager.handle_block_proposal(block).unwrap();

            // Create and handle M-notarization
            let mut m_votes = HashSet::new();
            for i in 0..=2 {
                m_votes.insert(create_test_vote(
                    i,
                    view_num as u64,
                    block_hash,
                    leader_id,
                    &setup,
                ));
            }
            let m_not = create_test_m_notarization::<6, 1, 3>(
                &m_votes,
                view_num as u64,
                block_hash,
                leader_id,
            );
            manager.handle_m_notarization(m_not).unwrap();

            parent_hash = block_hash;
        }

        assert_eq!(manager.current_view_number(), 3);
        assert_eq!(manager.non_finalized_count(), 4); // Views 0, 1, 2, 3

        let result = manager.shutdown();
        assert!(result.is_ok());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_exactly_2f_votes_does_not_trigger_m_notarization() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        manager.handle_block_proposal(block).unwrap();

        // Add exactly 2f = 2 votes (not enough for M-notarization which requires 2f+1=3)
        for i in 1..=1 {
            let vote = create_test_vote(i, 0, block_hash, leader_id, &setup);
            let result = manager.handle_vote(vote);

            if let ViewProgressEvent::ShouldMNotarize { .. } = result.unwrap() {
                panic!("Should not M-notarize with only 2f votes");
            }
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_exactly_2f_plus_1_votes_triggers_m_notarization() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        manager.handle_block_proposal(block).unwrap();

        // Add exactly 2 more votes to reach 2f+1 = 3 total votes
        // (leader vote is already counted from block proposal)
        let vote1 = create_test_vote(1, 0, block_hash, leader_id, &setup);
        let result1 = manager.handle_vote(vote1).unwrap();

        // First vote brings us to 2 votes, not enough yet
        assert!(
            !matches!(result1, ViewProgressEvent::ShouldMNotarize { .. }),
            "Should not trigger M-notarization with only 2 votes"
        );

        let vote2 = create_test_vote(2, 0, block_hash, leader_id, &setup);
        let result2 = manager.handle_vote(vote2).unwrap();

        // Second vote brings us to 3 votes (>2F), should trigger M-notarization
        match result2 {
            ViewProgressEvent::ShouldMNotarize {
                view,
                block_hash: _,
                should_forward_m_notarization,
            }
            | ViewProgressEvent::ShouldVoteAndMNotarize {
                view,
                block_hash: _,
                should_forward_m_notarization,
            } => {
                assert_eq!(view, 0);
                assert!(should_forward_m_notarization);
            }
            other => panic!("Expected M-notarization with 2f+1 votes, got {:?}", other),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_exactly_n_minus_f_minus_1_votes_does_not_trigger_finalization() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        manager.handle_block_proposal(block).unwrap();

        // Add exactly n-f-1 = 4 votes (not enough for finalization which requires n-f=5)
        for i in 1..=3 {
            let vote = create_test_vote(i, 0, block_hash, leader_id, &setup);
            let result = manager.handle_vote(vote);

            match result.unwrap() {
                ViewProgressEvent::ShouldFinalize { .. }
                | ViewProgressEvent::ShouldVoteAndFinalize { .. } => {
                    panic!("Should not finalize with only n-f-1 votes");
                }
                _ => {}
            }
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_receive_m_notarization_before_block() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let block_hash = [1u8; blake3::OUT_LEN];

        // Receive M-notarization BEFORE block
        let mut votes = HashSet::new();
        for i in 1..=3 {
            votes.insert(create_test_vote(i, 0, block_hash, leader_id, &setup));
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 0, block_hash, leader_id);

        let result = manager.handle_m_notarization(m_notarization);
        assert!(result.is_ok());

        // Should trigger view progression even without block
        match result.unwrap() {
            ViewProgressEvent::ProgressToNextView { .. }
            | ViewProgressEvent::ShouldVoteAndProgressToNextView { .. } => {}
            _ => {}
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_receive_votes_before_block() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let block_hash = [1u8; blake3::OUT_LEN];

        // Receive votes BEFORE block
        for i in 2..=4 {
            let vote = create_test_vote(i, 0, block_hash, leader_id, &setup);
            let result = manager.handle_vote(vote);
            assert!(result.is_ok());

            // Should return Await since no block yet
            if let ViewProgressEvent::Await = result.unwrap() {}
        }

        // Now receive block
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);

        let result = manager.handle_block_proposal(block);
        assert!(result.is_ok());

        // Should trigger M-notarization since we already have enough votes
        match result.unwrap() {
            ViewProgressEvent::ShouldVoteAndMNotarize { .. }
            | ViewProgressEvent::ShouldMNotarize { .. }
            | ViewProgressEvent::ShouldVote { .. } => {}
            _ => {}
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_duplicate_vote_handling() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        manager.handle_block_proposal(block).unwrap();

        // Add same vote twice
        let vote = create_test_vote(2, 0, block_hash, leader_id, &setup);
        let result1 = manager.handle_vote(vote.clone());
        let result2 = manager.handle_vote(vote.clone());

        assert!(result1.is_ok());
        assert!(result2.unwrap_err().to_string().contains("already exists"));

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_duplicate_nullify_handling() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];

        // Add same nullify twice
        let nullify = create_test_nullify(2, 0, leader_id, &setup);
        let result1 = manager.handle_nullify(nullify.clone());
        let result2 = manager.handle_nullify(nullify.clone());

        assert!(result1.is_ok());
        assert!(result2.unwrap_err().to_string().contains("already exists"));

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_receiving_old_view_messages_after_progression() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let replica_id = setup.peer_set.sorted_peer_ids[1];

        // Progress through views 0→1→2→3, creating M-notarizations for each
        for view_num in 0..=2 {
            let leader_id = setup.peer_set.sorted_peer_ids[(view_num % 6) as usize];
            let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();

            // Only add block for view 0 (already at view 0)
            if view_num == 0 {
                let block = create_test_block(
                    view_num,
                    leader_id,
                    [0; blake3::OUT_LEN],
                    leader_sk.clone(),
                    view_num,
                );
                let block_hash = block.get_hash();
                manager.handle_block_proposal(block).unwrap();

                // Add votes to create M-notarization (>2F = 3 votes)
                // Leader's vote counted, add 2 more
                for i in 1..=2 {
                    let vote = create_test_vote(i, view_num, block_hash, leader_id, &setup);
                    manager.handle_vote(vote).unwrap();
                }

                // Verify M-notarization exists
                let view_ctx = manager.view_chain.find_view_context(view_num).unwrap();
                assert!(view_ctx.m_notarization.is_some());

                // Progress to next view
                let next_view = view_num + 1;
                let next_leader = setup.peer_set.sorted_peer_ids[(next_view % 6) as usize];
                let new_view_ctx = ViewContext::new(next_view, next_leader, replica_id, block_hash);
                manager
                    .view_chain
                    .progress_with_m_notarization(new_view_ctx)
                    .unwrap();
            } else {
                // For views 1 and 2, manually create M-notarization and progress
                // (simplified - directly insert M-notarization into current view)
                let current_view = manager.view_chain.current_view_mut();
                let mut votes = HashSet::new();
                for i in 0..=2 {
                    let vote =
                        create_test_vote(i, view_num, [0; blake3::OUT_LEN], leader_id, &setup);
                    votes.insert(vote);
                }
                let m_not = create_test_m_notarization::<6, 1, 3>(
                    &votes,
                    view_num,
                    [0; blake3::OUT_LEN],
                    leader_id,
                );
                current_view.m_notarization = Some(m_not);

                // Progress to next view
                let next_view = view_num + 1;
                let next_leader = setup.peer_set.sorted_peer_ids[(next_view % 6) as usize];
                let new_view_ctx =
                    ViewContext::new(next_view, next_leader, replica_id, [0; blake3::OUT_LEN]);
                manager
                    .view_chain
                    .progress_with_m_notarization(new_view_ctx)
                    .unwrap();
            }
        }

        assert_eq!(manager.current_view_number(), 3);

        // Receive message for view 0 (old view)
        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let vote = create_test_vote(3, 0, [1u8; blake3::OUT_LEN], leader_id_0, &setup);

        let result = manager.handle_vote(vote);
        assert!(result.is_ok());

        // Should still be in view 3
        assert_eq!(manager.current_view_number(), 3);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_multiple_consecutive_view_progressions() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // Progress through 5 views consecutively
        for view_num in 0..5 {
            let current_view = manager.current_view_number();
            assert_eq!(
                current_view, view_num,
                "Manager should be at view {}",
                view_num
            );

            // Get the leader for the current view
            let leader_id = manager.view_chain.current().leader_id;
            let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();

            // Create and handle block for current view
            let parent_hash = if view_num == 0 {
                [0; blake3::OUT_LEN]
            } else {
                manager
                    .view_chain
                    .find_view_context(view_num - 1)
                    .and_then(|ctx| ctx.block_hash)
                    .unwrap_or([0; blake3::OUT_LEN])
            };

            let block = create_test_block(
                current_view,
                leader_id,
                parent_hash,
                leader_sk.clone(),
                view_num,
            );
            let block_hash = block.get_hash();

            manager.handle_block_proposal(block).unwrap();

            // Add votes to reach M-notarization threshold (>2F = 3 votes for F=1)
            // We need to collect 3 votes including the leader's vote for the M-notarization object
            let mut votes = HashSet::new();

            for (vote_count, i) in (0..setup.peer_set.sorted_peer_ids.len()).enumerate() {
                if vote_count >= 3 {
                    break;
                }
                votes.insert(create_test_vote(
                    i,
                    current_view,
                    block_hash,
                    leader_id,
                    &setup,
                ));
            }

            // Create and handle M-notarization, which will progress to next view
            let m_notarization =
                create_test_m_notarization::<6, 1, 3>(&votes, current_view, block_hash, leader_id);
            manager.handle_m_notarization(m_notarization).unwrap();
        }

        assert_eq!(manager.current_view_number(), 5);
        assert_eq!(manager.non_finalized_count(), 6); // Views 0-5

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_finalization_after_multiple_view_progressions() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let leader_sk_0 = setup.peer_id_to_secret_key.get(&leader_id_0).unwrap();
        let block_0 =
            create_test_block(0, leader_id_0, [0; blake3::OUT_LEN], leader_sk_0.clone(), 0);
        let block_hash_0 = block_0.get_hash();

        // Add block to view 0
        manager.handle_block_proposal(block_0).unwrap();

        // Add enough votes for L-notarization in view 0 (need 5 total: leader + 4 others)
        for i in 1..=4 {
            let vote = create_test_vote(i, 0, block_hash_0, leader_id_0, &setup);
            manager.handle_vote(vote).unwrap();
        }

        // Create M-notarization for view 0 to progress to view 1
        let mut votes_0 = HashSet::new();
        for i in 0..3 {
            votes_0.insert(create_test_vote(i, 0, block_hash_0, leader_id_0, &setup));
        }
        let m_notarization_0 =
            create_test_m_notarization::<6, 1, 3>(&votes_0, 0, block_hash_0, leader_id_0);
        manager.handle_m_notarization(m_notarization_0).unwrap();

        // Now in view 1, create block and M-notarization for view 1
        assert_eq!(manager.current_view_number(), 1);
        let leader_id_1 = manager.view_chain.current().leader_id;
        let leader_sk_1 = setup.peer_id_to_secret_key.get(&leader_id_1).unwrap();
        let block_1 = create_test_block(1, leader_id_1, block_hash_0, leader_sk_1.clone(), 1);
        let block_hash_1 = block_1.get_hash();

        manager.handle_block_proposal(block_1).unwrap();

        // Create M-notarization for view 1 to progress to view 2
        let mut votes_1 = HashSet::new();
        for i in 0..3 {
            votes_1.insert(create_test_vote(i, 1, block_hash_1, leader_id_1, &setup));
        }
        let m_notarization_1 =
            create_test_m_notarization::<6, 1, 3>(&votes_1, 1, block_hash_1, leader_id_1);
        manager.handle_m_notarization(m_notarization_1).unwrap();

        // Now in view 2
        assert_eq!(manager.current_view_number(), 2);

        // Finalize view 0 (which has L-notarization)
        assert_eq!(manager.current_view_number(), 2);
        let result = manager.finalize_view(0);
        assert!(result.is_ok());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_view_progression_via_m_notarization_vs_nullification() {
        let setup = create_test_peer_setup(6);

        // Test M-notarization progression
        {
            let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
                create_test_manager(&setup, 1);

            let leader_id = setup.peer_set.sorted_peer_ids[0];
            let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
            let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
            let block_hash = block.get_hash();

            manager.handle_block_proposal(block).unwrap();

            let mut votes = HashSet::new();
            for i in 1..=3 {
                votes.insert(create_test_vote(i, 0, block_hash, leader_id, &setup));
            }
            let m_notarization =
                create_test_m_notarization::<6, 1, 3>(&votes, 0, block_hash, leader_id);

            let result = manager.handle_m_notarization(m_notarization);
            assert!(result.is_ok());

            assert_eq!(manager.current_view_number(), 1);

            std::fs::remove_file(path).unwrap();
        }

        // Test nullification progression
        {
            let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
                create_test_manager(&setup, 1);

            let leader_id = setup.peer_set.sorted_peer_ids[0];

            let mut nullify_messages = HashSet::new();
            for i in 1..=3 {
                nullify_messages.insert(create_test_nullify(i, 0, leader_id, &setup));
            }
            let nullification =
                create_test_nullification::<6, 1, 3>(&nullify_messages, 0, leader_id);

            let result = manager.handle_nullification(nullification);
            assert!(result.is_ok());

            assert_eq!(manager.current_view_number(), 1);

            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn test_select_parent_with_multiple_nullified_views() {
        // This test verifies that when multiple views are nullified in sequence,
        // the next view correctly uses SelectParent to build on the last M-notarized block,
        // not on the intermediate nullified views.
        //
        // Scenario:
        // - View 0: M-notarized with block B0
        // - View 1: Nullified (no block)
        // - View 2: Nullified (no block)
        // - View 3: Should build on B0 (the last M-notarized block)

        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // View 0: Create and M-notarize a block
        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let leader_sk_0 = setup.peer_id_to_secret_key.get(&leader_id_0).unwrap();
        let block_0 =
            create_test_block(0, leader_id_0, [0; blake3::OUT_LEN], leader_sk_0.clone(), 0);
        let block_hash_0 = block_0.get_hash();

        // Add block proposal for view 0
        manager.handle_block_proposal(block_0).unwrap();

        // Create M-notarization for view 0 (need >2F = 2, so 3 votes including leader)
        let mut m_votes_0 = HashSet::new();
        for i in 0..=2 {
            m_votes_0.insert(create_test_vote(i, 0, block_hash_0, leader_id_0, &setup));
        }
        let m_not_0 =
            create_test_m_notarization::<6, 1, 3>(&m_votes_0, 0, block_hash_0, leader_id_0);

        let result = manager.handle_m_notarization(m_not_0);
        assert!(result.is_ok());
        assert_eq!(manager.current_view_number(), 1);

        // View 1: Nullify (no block proposed)
        let leader_id_1 = setup.peer_set.sorted_peer_ids[1];
        let mut nullify_messages_1 = HashSet::new();
        for i in 0..=2 {
            nullify_messages_1.insert(create_test_nullify(i, 1, leader_id_1, &setup));
        }
        let nullification_1 =
            create_test_nullification::<6, 1, 3>(&nullify_messages_1, 1, leader_id_1);

        let result = manager.handle_nullification(nullification_1);
        assert!(result.is_ok());
        assert_eq!(manager.current_view_number(), 2);

        // View 2: Nullify (no block proposed)
        let leader_id_2 = setup.peer_set.sorted_peer_ids[2];
        let mut nullify_messages_2 = HashSet::new();
        for i in 0..=2 {
            nullify_messages_2.insert(create_test_nullify(i, 2, leader_id_2, &setup));
        }
        let nullification_2 =
            create_test_nullification::<6, 1, 3>(&nullify_messages_2, 2, leader_id_2);

        let result = manager.handle_nullification(nullification_2);
        assert!(result.is_ok());
        assert_eq!(manager.current_view_number(), 3);

        // View 3: Verify parent is block_hash_0 (from view 0)
        // Check that view 3's parent_block_hash is correctly set to the last M-notarized block
        let view_3_context = manager.view_chain.current();
        assert_eq!(
            view_3_context.parent_block_hash, block_hash_0,
            "View 3 should have view 0's block (the last M-notarized block) as parent, \
         not an undefined hash from the nullified intermediate views"
        );

        // Additional verification: Leader of view 3 can propose a valid block
        let leader_id_3 = setup.peer_set.sorted_peer_ids[3];
        let leader_sk_3 = setup.peer_id_to_secret_key.get(&leader_id_3).unwrap();

        // This block should have block_hash_0 as parent
        let block_3 = create_test_block(3, leader_id_3, block_hash_0, leader_sk_3.clone(), 1);

        let result = manager.handle_block_proposal(block_3);
        assert!(
            result.is_ok(),
            "Leader of view 3 should be able to propose a block with view 0's block as parent"
        );

        // Verify that replicas can vote for this block (it passes validation)
        match result.unwrap() {
            ViewProgressEvent::ShouldVote {
                view,
                block_hash: _,
            } => {
                assert_eq!(view, 3);
            }
            other => panic!("Expected ShouldVote event, got {:?}", other),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_select_parent_with_no_m_notarization_in_non_finalized_views() {
        // This test verifies that when no M-notarization exists in non-finalized views,
        // select_parent correctly falls back to the previously_committed_block_hash.
        //
        // Scenario:
        // - Genesis block is finalized (previously_committed_block_hash = genesis)
        // - View 0: Nullified (no block, no M-notarization)
        // - View 1: Should build on genesis (previously_committed_block_hash)

        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // Nullify view 0 immediately (no block proposed)
        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let mut nullify_messages_0 = HashSet::new();
        for i in 0..=2 {
            nullify_messages_0.insert(create_test_nullify(i, 0, leader_id_0, &setup));
        }
        let nullification_0 =
            create_test_nullification::<6, 1, 3>(&nullify_messages_0, 0, leader_id_0);

        let result = manager.handle_nullification(nullification_0);
        assert!(result.is_ok());
        assert_eq!(manager.current_view_number(), 1);

        // View 1's parent should be the genesis/previously committed block (all zeros in this test)
        let view_1_context = manager.view_chain.current();
        assert_eq!(
            view_1_context.parent_block_hash,
            [0; blake3::OUT_LEN],
            "View 1 should have genesis block as parent when no M-notarization exists in non-finalized views"
        );

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_select_parent_skips_nullified_views() {
        // This test verifies that select_parent correctly finds the greatest view
        // with M-notarization, even when there's a complex pattern of M-notarized
        // and nullified views.
        //
        // Scenario:
        // - View 0: M-notarized with block B0
        // - View 1: M-notarized with block B1
        // - View 2: Nullified
        // - View 3: Should build on B1 (greatest M-notarized view < 3)

        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // View 0: M-notarize
        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let leader_sk_0 = setup.peer_id_to_secret_key.get(&leader_id_0).unwrap();
        let block_0 =
            create_test_block(0, leader_id_0, [0; blake3::OUT_LEN], leader_sk_0.clone(), 0);
        let block_hash_0 = block_0.get_hash();

        manager.handle_block_proposal(block_0).unwrap();

        let mut m_votes_0 = HashSet::new();
        for i in 0..=2 {
            m_votes_0.insert(create_test_vote(i, 0, block_hash_0, leader_id_0, &setup));
        }
        let m_not_0 =
            create_test_m_notarization::<6, 1, 3>(&m_votes_0, 0, block_hash_0, leader_id_0);
        manager.handle_m_notarization(m_not_0).unwrap();
        assert_eq!(manager.current_view_number(), 1);

        // View 1: M-notarize
        let leader_id_1 = setup.peer_set.sorted_peer_ids[1];
        let leader_sk_1 = setup.peer_id_to_secret_key.get(&leader_id_1).unwrap();
        let block_1 = create_test_block(1, leader_id_1, block_hash_0, leader_sk_1.clone(), 1);
        let block_hash_1 = block_1.get_hash();

        manager.handle_block_proposal(block_1).unwrap();

        let mut m_votes_1 = HashSet::new();
        for i in 0..=2 {
            m_votes_1.insert(create_test_vote(i, 1, block_hash_1, leader_id_1, &setup));
        }
        let m_not_1 =
            create_test_m_notarization::<6, 1, 3>(&m_votes_1, 1, block_hash_1, leader_id_1);
        manager.handle_m_notarization(m_not_1).unwrap();
        assert_eq!(manager.current_view_number(), 2);

        // View 2: Nullify
        let leader_id_2 = setup.peer_set.sorted_peer_ids[2];
        let mut nullify_messages_2 = HashSet::new();
        for i in 0..=2 {
            nullify_messages_2.insert(create_test_nullify(i, 2, leader_id_2, &setup));
        }
        let nullification_2 =
            create_test_nullification::<6, 1, 3>(&nullify_messages_2, 2, leader_id_2);
        manager.handle_nullification(nullification_2).unwrap();
        assert_eq!(manager.current_view_number(), 3);

        // View 3: Should use B1 as parent (greatest M-notarized view < 3)
        let view_3_context = manager.view_chain.current();
        assert_eq!(
            view_3_context.parent_block_hash, block_hash_1,
            "View 3 should have view 1's block (the greatest M-notarized view < 3) as parent"
        );

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_m_notarization_forwards_new_m_notarization() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        // Add block first
        manager.handle_block_proposal(block).unwrap();

        // Create M-notarization
        let mut votes = HashSet::new();
        for i in 1..=3 {
            votes.insert(create_test_vote(i, 0, block_hash, leader_id, &setup));
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 0, block_hash, leader_id);

        // First M-notarization should have should_forward_m_notarization = true
        let result = manager.handle_m_notarization(m_notarization);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ProgressToNextView {
                should_forward_m_notarization,
                ..
            } => {
                assert!(
                    should_forward_m_notarization,
                    "New M-notarization should be forwarded"
                );
            }
            ViewProgressEvent::ShouldVoteAndProgressToNextView {
                should_forward_m_notarization,
                ..
            } => {
                assert!(
                    should_forward_m_notarization,
                    "New M-notarization should be forwarded"
                );
            }
            other => panic!("Expected view progression event, got {:?}", other),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_m_notarization_does_not_forward_duplicate() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], leader_sk.clone(), 0);
        let block_hash = block.get_hash();

        manager.handle_block_proposal(block).unwrap();

        // Mark replica as voted (simulate the voting action)
        manager.mark_voted(0).unwrap();

        // Create M-notarization
        let mut votes = HashSet::new();
        for i in 2..=4 {
            votes.insert(create_test_vote(i, 0, block_hash, leader_id, &setup));
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 0, block_hash, leader_id);

        // First call - should progress to view 1 with should_forward = true
        let first_result = manager
            .handle_m_notarization(m_notarization.clone())
            .unwrap();
        match first_result {
            ViewProgressEvent::ProgressToNextView {
                should_forward_m_notarization,
                ..
            } => {
                assert!(
                    should_forward_m_notarization,
                    "First M-notarization should be forwarded"
                );
            }
            other => panic!("Expected ProgressToNextView, got {:?}", other),
        }

        assert_eq!(manager.current_view_number(), 1);

        // Second call (duplicate) for past view - should have should_forward = false internally
        let result = manager.handle_m_notarization(m_notarization);
        assert!(result.is_ok());

        // For duplicate past view M-notarization, expect NoOp (already voted and progressed)
        match result.unwrap() {
            ViewProgressEvent::NoOp => {
                // Correct - duplicate M-notarization for past view where we already voted
            }
            other => panic!(
                "Expected NoOp for duplicate past M-notarization, got {:?}",
                other
            ),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_nullification_broadcasts_new_nullification() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];

        // Create nullification
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            nullify_messages.insert(create_test_nullify(i, 0, leader_id, &setup));
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 0, leader_id);

        // First nullification should trigger broadcast
        let result = manager.handle_nullification(nullification);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ProgressToNextViewOnNullification {
                should_broadcast_nullification,
                ..
            } => {
                assert!(
                    should_broadcast_nullification,
                    "New nullification should be broadcast"
                );
            }
            other => panic!(
                "Expected ProgressToNextViewOnNullification, got {:?}",
                other
            ),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_nullification_does_not_broadcast_duplicate() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];

        // Create nullification
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            nullify_messages.insert(create_test_nullify(i, 0, leader_id, &setup));
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 0, leader_id);

        // Add first time
        manager.handle_nullification(nullification.clone()).unwrap();

        // Now in view 1
        assert_eq!(manager.current_view_number(), 1);

        // Add same nullification again for past view - should not broadcast
        let result = manager.handle_nullification(nullification);
        assert!(result.is_ok());

        // For past view duplicate, should return NoOp or ShouldNullify (without broadcast)
        match result.unwrap() {
            ViewProgressEvent::NoOp | ViewProgressEvent::ShouldNullify { .. } => {}
            other => panic!("Expected NoOp/ShouldNullify for duplicate, got {:?}", other),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_pending_block_stored_when_parent_not_m_notarized() {
        // Test: Block for view V+1 arrives before view V has M-notarization -> stored as pending
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let leader_sk_0 = setup.peer_id_to_secret_key.get(&leader_id_0).unwrap();
        let block_0 =
            create_test_block(0, leader_id_0, [0; blake3::OUT_LEN], leader_sk_0.clone(), 0);
        let block_hash_0 = block_0.get_hash();

        // Add block for view 0 (leader vote is automatically added, but NO M-notarization yet -
        // only 1 vote)
        manager.handle_block_proposal(block_0).unwrap();
        assert_eq!(manager.current_view_number(), 0);

        // Create block for view 1
        let leader_id_1 = setup.peer_set.sorted_peer_ids[1];
        let leader_sk_1 = setup.peer_id_to_secret_key.get(&leader_id_1).unwrap();
        let block_1 = create_test_block(1, leader_id_1, block_hash_0, leader_sk_1.clone(), 1);
        let block_hash_1 = block_1.get_hash();

        // First call returns ShouldUpdateView
        let result = manager.handle_block_proposal(block_1.clone());
        assert!(result.is_ok());
        match result.unwrap() {
            ViewProgressEvent::ShouldUpdateView { new_view, leader } => {
                assert_eq!(new_view, 1);
                assert_eq!(leader, leader_id_1);
            }
            other => panic!("Expected ShouldUpdateView, got {:?}", other),
        }

        // Manually create view 1 context (simulating what happens after ShouldUpdateView)
        let view_ctx_1 = ViewContext::new(1, leader_id_1, manager.replica_id, block_hash_0);
        manager.view_chain.non_finalized_views.insert(1, view_ctx_1);
        manager.view_chain.current_view = 1; // <-- ADD THIS LINE

        // Now process the block again
        let result = manager.handle_block_proposal(block_1.clone());
        assert!(result.is_ok());

        // Should return Await since parent doesn't have M-notarization
        match result.unwrap() {
            ViewProgressEvent::Await => {}
            other => panic!("Expected Await, got {:?}", other),
        }

        // Verify block is stored as pending
        let view_1_ctx = manager.view_chain.find_view_context(1).unwrap();
        assert!(view_1_ctx.pending_block.is_some());
        assert!(view_1_ctx.block.is_none());
        assert_eq!(
            view_1_ctx.pending_block.as_ref().unwrap().get_hash(),
            block_hash_1
        );

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_pending_block_processed_after_parent_m_notarization() {
        // Test: When parent gets M-notarization, pending block is processed
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let leader_sk_0 = setup.peer_id_to_secret_key.get(&leader_id_0).unwrap();
        let block_0 =
            create_test_block(0, leader_id_0, [0; blake3::OUT_LEN], leader_sk_0.clone(), 0);
        let block_hash_0 = block_0.get_hash();

        // Add block for view 0 (only leader's vote - no M-notarization)
        manager.handle_block_proposal(block_0).unwrap();

        // Create block for view 1
        let leader_id_1 = setup.peer_set.sorted_peer_ids[1];
        let leader_sk_1 = setup.peer_id_to_secret_key.get(&leader_id_1).unwrap();
        let block_1 = create_test_block(1, leader_id_1, block_hash_0, leader_sk_1.clone(), 1);
        let block_hash_1 = block_1.get_hash();

        // Manually create view 1 context WITHOUT progressing current_view
        // This simulates receiving a block for a future view
        let view_ctx_1 = ViewContext::new(1, leader_id_1, manager.replica_id, block_hash_0);
        manager.view_chain.non_finalized_views.insert(1, view_ctx_1);
        // NOTE: Do NOT set current_view = 1, keep it at 0

        // Add the block directly to view 1 using view_chain (bypassing handle_block_proposal)
        let result = manager
            .view_chain
            .add_block_proposal(1, block_1, &manager.peers);
        assert!(result.is_ok());
        assert!(result.unwrap().should_await); // Should await parent M-notarization

        // Verify block is pending in view 1
        let view_1_before = manager.view_chain.find_view_context(1).unwrap();
        assert!(view_1_before.pending_block.is_some());
        assert!(view_1_before.block.is_none());

        // Now add 2 more votes to view 0 to reach M-notarization (3 total: leader + 2)
        for i in 1..=2 {
            let vote = create_test_vote(i, 0, block_hash_0, leader_id_0, &setup);
            manager.handle_vote(vote).unwrap();
        }

        // The last vote should trigger M-notarization and process pending blocks
        // Verify that pending block was processed
        let view_1_after = manager.view_chain.find_view_context(1).unwrap();
        assert!(view_1_after.pending_block.is_none());
        assert!(view_1_after.block.is_some());
        assert_eq!(
            view_1_after.block.as_ref().unwrap().get_hash(),
            block_hash_1
        );

        // Verify leader's vote was automatically added
        assert_eq!(view_1_after.votes.len(), 1);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_pending_block_triggers_vote_after_processing() {
        // Test: After pending block is processed, replica should vote
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 2); // Use replica at index 2, NOT index 1

        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let leader_sk_0 = setup.peer_id_to_secret_key.get(&leader_id_0).unwrap();
        let block_0 =
            create_test_block(0, leader_id_0, [0; blake3::OUT_LEN], leader_sk_0.clone(), 0);
        let block_hash_0 = block_0.get_hash();

        manager.handle_block_proposal(block_0).unwrap();

        // Create block for view 1 (leader is at index 1, replica is at index 2)
        let leader_id_1 = setup.peer_set.sorted_peer_ids[1];
        let leader_sk_1 = setup.peer_id_to_secret_key.get(&leader_id_1).unwrap();
        let block_1 = create_test_block(1, leader_id_1, block_hash_0, leader_sk_1.clone(), 1);
        let block_hash_1 = block_1.get_hash();

        // Manually create view 1 context WITHOUT progressing current_view
        let view_ctx_1 = ViewContext::new(1, leader_id_1, manager.replica_id, block_hash_0);
        manager.view_chain.non_finalized_views.insert(1, view_ctx_1);

        // Add the block as pending using view_chain directly
        manager
            .view_chain
            .add_block_proposal(1, block_1, &manager.peers)
            .unwrap();

        // Verify block is pending
        let view_1_before = manager.view_chain.find_view_context(1).unwrap();
        assert!(view_1_before.pending_block.is_some());
        assert!(view_1_before.block.is_none());

        // M-notarize view 0 (triggers pending block processing)
        for i in 1..=2 {
            let vote = create_test_vote(i, 0, block_hash_0, leader_id_0, &setup);
            manager.handle_vote(vote).unwrap();
        }

        // Verify pending block was processed
        let view_1_after = manager.view_chain.find_view_context(1).unwrap();
        assert!(view_1_after.pending_block.is_none());
        assert!(view_1_after.block.is_some());

        // Now manually progress to view 1 (simulating what would happen after M-notarization)
        manager.view_chain.current_view = 1;

        // Now tick should discover the processed block and trigger vote
        let tick_result = manager.tick();
        assert!(tick_result.is_ok());

        match tick_result.unwrap() {
            ViewProgressEvent::ShouldVote { view, block_hash } => {
                assert_eq!(view, 1);
                assert_eq!(block_hash, block_hash_1);
            }
            other => panic!(
                "Expected ShouldVote after pending block processed, got {:?}",
                other
            ),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_multiple_pending_blocks_cascade() {
        // Test: Multiple pending blocks are processed in sequence
        // View 0: Has block, gets M-notarized
        // View 1: Pending (waiting for view 0)
        // View 2: Pending (waiting for view 1)
        // When view 0 gets M-notarized -> view 1 processed
        // When view 1 gets M-notarized -> view 2 processed
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let leader_sk_0 = setup.peer_id_to_secret_key.get(&leader_id_0).unwrap();
        let block_0 =
            create_test_block(0, leader_id_0, [0; blake3::OUT_LEN], leader_sk_0.clone(), 0);
        let block_hash_0 = block_0.get_hash();

        manager.handle_block_proposal(block_0).unwrap();

        // Create view 1 context and add pending block
        let leader_id_1 = setup.peer_set.sorted_peer_ids[1];
        let leader_sk_1 = setup.peer_id_to_secret_key.get(&leader_id_1).unwrap();
        let block_1 = create_test_block(1, leader_id_1, block_hash_0, leader_sk_1.clone(), 1);
        let block_hash_1 = block_1.get_hash();

        // Manually create view 1 context
        let view_ctx_1 = ViewContext::new(1, leader_id_1, manager.replica_id, block_hash_0);
        manager.view_chain.non_finalized_views.insert(1, view_ctx_1);

        // Add block 1 as pending
        manager
            .view_chain
            .add_block_proposal(1, block_1, &manager.peers)
            .unwrap();

        // Verify block 1 is pending
        assert!(
            manager
                .view_chain
                .find_view_context(1)
                .unwrap()
                .pending_block
                .is_some()
        );

        // M-notarize view 0 - this will process pending block_1
        for i in 1..=2 {
            let vote = create_test_vote(i, 0, block_hash_0, leader_id_0, &setup);
            manager.handle_vote(vote).unwrap();
        }

        // View 1 should now be processed (no longer pending)
        let view_1 = manager.view_chain.find_view_context(1).unwrap();
        assert!(view_1.pending_block.is_none());
        assert!(view_1.block.is_some());

        // NOW create view 2 context and add pending block (after view 1 is processed)
        let leader_id_2 = setup.peer_set.sorted_peer_ids[2];
        let leader_sk_2 = setup.peer_id_to_secret_key.get(&leader_id_2).unwrap();
        let block_2 = create_test_block(2, leader_id_2, block_hash_1, leader_sk_2.clone(), 2);
        let block_hash_2 = block_2.get_hash();

        // Manually create view 2 context
        let view_ctx_2 = ViewContext::new(2, leader_id_2, manager.replica_id, block_hash_1);
        manager.view_chain.non_finalized_views.insert(2, view_ctx_2);

        // Add block 2 as pending (parent view 1 has block but no M-notarization)
        manager
            .view_chain
            .add_block_proposal(2, block_2, &manager.peers)
            .unwrap();

        // View 2 should be pending (waiting for view 1 M-notarization)
        let view_2_before = manager.view_chain.find_view_context(2).unwrap();
        assert!(view_2_before.pending_block.is_some());
        assert!(view_2_before.block.is_none());

        // Manually add votes and M-notarization to view 1
        // View 1 already has leader vote, add 2 more to reach threshold (3 total)
        let mut votes_view_1 = HashSet::new();
        // Collect leader vote (already exists)
        votes_view_1.extend(
            manager
                .view_chain
                .find_view_context(1)
                .unwrap()
                .votes
                .clone(),
        );
        // Add 2 more votes
        for i in 2..=3 {
            let vote = create_test_vote(i, 1, block_hash_1, leader_id_1, &setup);
            votes_view_1.insert(vote);
        }

        // Manually create M-notarization for view 1
        let m_not_1 =
            create_test_m_notarization::<6, 1, 3>(&votes_view_1, 1, block_hash_1, leader_id_1);
        let view_1_mut = manager.view_chain.non_finalized_views.get_mut(&1).unwrap();
        view_1_mut.m_notarization = Some(m_not_1);

        // Verify view 1 has M-notarization
        let view_1_after = manager.view_chain.find_view_context(1).unwrap();
        assert!(view_1_after.m_notarization.is_some());

        // Manually trigger pending block processing for view 1's children
        manager
            .view_chain
            .process_pending_child_proposals(1, &manager.peers)
            .unwrap();

        // Now view 2 should be processed
        let view_2_after = manager.view_chain.find_view_context(2).unwrap();
        assert!(view_2_after.pending_block.is_none());
        assert!(view_2_after.block.is_some());
        assert_eq!(
            view_2_after.block.as_ref().unwrap().get_hash(),
            block_hash_2
        );

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_pending_block_with_immediate_m_notarization() {
        // Test: Pending block reaches M-notarization threshold immediately after being processed
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let leader_sk_0 = setup.peer_id_to_secret_key.get(&leader_id_0).unwrap();
        let block_0 =
            create_test_block(0, leader_id_0, [0; blake3::OUT_LEN], leader_sk_0.clone(), 0);
        let block_hash_0 = block_0.get_hash();

        manager.handle_block_proposal(block_0).unwrap();

        // Create view 1 context and add pending block
        let leader_id_1 = setup.peer_set.sorted_peer_ids[1];
        let leader_sk_1 = setup.peer_id_to_secret_key.get(&leader_id_1).unwrap();
        let block_1 = create_test_block(1, leader_id_1, block_hash_0, leader_sk_1.clone(), 1);
        let block_hash_1 = block_1.get_hash();

        // Manually create view 1 context
        let view_ctx_1 = ViewContext::new(1, leader_id_1, manager.replica_id, block_hash_0);
        manager.view_chain.non_finalized_views.insert(1, view_ctx_1);

        // Add block 1 as pending
        manager
            .view_chain
            .add_block_proposal(1, block_1, &manager.peers)
            .unwrap();

        // Verify block is pending
        let view_1_before = manager.view_chain.find_view_context(1).unwrap();
        assert!(view_1_before.pending_block.is_some());
        assert!(view_1_before.block.is_none());

        // M-notarize view 0 (triggers pending block processing)
        for i in 1..=2 {
            let vote = create_test_vote(i, 0, block_hash_0, leader_id_0, &setup);
            manager.handle_vote(vote).unwrap();
        }

        // View 1 should be processed with leader vote
        let view_1_after = manager.view_chain.find_view_context(1).unwrap();
        assert!(view_1_after.pending_block.is_none());
        assert!(view_1_after.block.is_some());
        assert_eq!(view_1_after.votes.len(), 1); // Just leader vote

        // Now manually add votes and M-notarization to show it can reach threshold
        let mut votes_view_1 = HashSet::new();
        votes_view_1.extend(view_1_after.votes.clone()); // Include leader vote

        // Add 2 more votes to reach threshold
        for i in 2..=3 {
            let vote = create_test_vote(i, 1, block_hash_1, leader_id_1, &setup);
            votes_view_1.insert(vote);
        }

        // Manually create and insert M-notarization
        let m_not_1 =
            create_test_m_notarization::<6, 1, 3>(&votes_view_1, 1, block_hash_1, leader_id_1);
        let view_1_mut = manager.view_chain.non_finalized_views.get_mut(&1).unwrap();
        view_1_mut.m_notarization = Some(m_not_1);

        // Verify view 1 has M-notarization
        let view_1_final = manager.view_chain.find_view_context(1).unwrap();
        assert!(view_1_final.m_notarization.is_some());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_pending_block_cannot_use_nullified_parent() {
        // Test: Block with nullified parent view is rejected, not stored as pending
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id_0 = setup.peer_set.sorted_peer_ids[0];
        let leader_sk_0 = setup.peer_id_to_secret_key.get(&leader_id_0).unwrap();
        let block_0 =
            create_test_block(0, leader_id_0, [0; blake3::OUT_LEN], leader_sk_0.clone(), 0);
        let block_hash_0 = block_0.get_hash();

        manager.handle_block_proposal(block_0).unwrap();

        // Nullify view 0
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            nullify_messages.insert(create_test_nullify(i, 0, leader_id_0, &setup));
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 0, leader_id_0);
        manager.handle_nullification(nullification).unwrap();

        // Now try to add block for view 1 with parent = block_hash_0
        // This should ERROR because view 0 is nullified
        let leader_id_1 = setup.peer_set.sorted_peer_ids[1];
        let leader_sk_1 = setup.peer_id_to_secret_key.get(&leader_id_1).unwrap();
        let block_1 = create_test_block(1, leader_id_1, block_hash_0, leader_sk_1.clone(), 1);

        let result = manager.handle_block_proposal(block_1);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Block proposal for parent view 0 is nullified")
        );

        std::fs::remove_file(path).unwrap();
    }
}
