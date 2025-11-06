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
            if conflicting_count >= 2 * F + 1 {
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

    /// Returns the replica's peer ID.
    pub fn replica_id(&self) -> PeerId {
        self.replica_id
    }

    /// Returns the number of non-finalized views in the chain.
    pub fn non_finalized_count(&self) -> usize {
        self.view_chain.non_finalized_count()
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
        } = self
            .view_chain
            .add_block_proposal(block_view_number, block)?;

        if should_await {
            return Ok(ViewProgressEvent::Await);
        }

        if is_enough_to_m_notarize && should_vote {
            return Ok(ViewProgressEvent::ShouldVoteAndMNotarize {
                view: block_view_number,
                block_hash,
            });
        }

        if is_enough_to_finalize && should_vote {
            return Ok(ViewProgressEvent::ShouldVoteAndFinalize {
                view: block_view_number,
                block_hash,
            });
        }

        if is_enough_to_m_notarize {
            return Ok(ViewProgressEvent::ShouldMNotarize {
                view: block_view_number,
                block_hash,
            });
        }

        if is_enough_to_finalize {
            return Ok(ViewProgressEvent::ShouldFinalize {
                view: block_view_number,
                block_hash,
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

        if is_enough_to_m_notarize && should_vote {
            return Ok(ViewProgressEvent::ShouldVoteAndMNotarize {
                view: vote_view_number,
                block_hash: vote_block_hash,
            });
        }

        if is_enough_to_finalize && should_vote {
            return Ok(ViewProgressEvent::ShouldVoteAndFinalize {
                view: vote_view_number,
                block_hash: vote_block_hash,
            });
        }

        if is_enough_to_m_notarize {
            return Ok(ViewProgressEvent::ShouldMNotarize {
                view: vote_view_number,
                block_hash: vote_block_hash,
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
                });
            } else {
                return Ok(ViewProgressEvent::ProgressToNextView {
                    new_view,
                    leader: new_leader,
                    notarized_block_hash: m_notarization_block_hash,
                });
            }
        }

        if should_notarize && should_vote {
            return Ok(ViewProgressEvent::ShouldVoteAndMNotarize {
                view: m_notarization_view_number,
                block_hash: m_notarization_block_hash,
            });
        }

        if should_notarize {
            // Process pending child blocks

            return Ok(ViewProgressEvent::ShouldMNotarize {
                view: m_notarization_view_number,
                block_hash: m_notarization_block_hash,
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
            let parent_hash = self.view_chain.current().parent_block_hash;
            let new_view_context =
                ViewContext::new(new_view, new_leader, self.replica_id, parent_hash);
            self.view_chain
                .progress_with_nullification(new_view_context)?;

            return Ok(ViewProgressEvent::ProgressToNextViewOnNullification {
                new_view,
                leader: new_leader,
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
        height: u64,
    ) -> Block {
        let transactions = vec![create_test_transaction()];
        Block::new(
            view,
            leader,
            parent_hash,
            transactions,
            1234567890,
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

    pub fn temp_db_path(suffix: &str) -> String {
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
        let setup = create_test_peer_setup(4);
        let (manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 0);

        assert_eq!(manager.current_view_context.view_number, 0);
        assert!(manager.unfinalized_view_context.is_none());
        assert_eq!(manager.peers.sorted_peer_ids.len(), 4);
        assert_eq!(manager.pending_txs.len(), 0);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_from_genesis_creates_manager_with_genesis_state() {
        let setup = create_test_peer_setup(4);
        let replica_id = setup.peer_set.sorted_peer_ids[0];
        let mut peer_strs: Vec<String> = Vec::with_capacity(setup.peer_set.sorted_peer_ids.len());
        for peer_id in &setup.peer_set.sorted_peer_ids {
            let pk = setup.peer_set.id_to_public_key.get(peer_id).unwrap();
            let mut buf = Vec::new();
            pk.0.serialize_compressed(&mut buf).unwrap();
            let peer_str = hex::encode(buf);
            peer_strs.push(peer_str);
        }
        let config = create_test_config(4, 1, peer_strs);

        let path = temp_db_path("view_manager");
        let persistence_storage = ConsensusStore::open(&path).unwrap();
        let manager: ViewProgressManager<4, 1, 3> =
            ViewProgressManager::from_genesis(config, replica_id, persistence_storage).unwrap();

        assert_eq!(manager.current_view_context.view_number, 0);
        assert!(manager.unfinalized_view_context.is_none());
        assert_eq!(manager.peers.sorted_peer_ids.len(), 4);
    }

    #[test]
    fn test_new_sets_correct_leader_for_view_zero() {
        let setup = create_test_peer_setup(4);
        let (manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // For round-robin, view 0 should have leader at index 0
        let expected_leader = setup.peer_set.sorted_peer_ids[0];
        assert_eq!(manager.current_view_context.leader_id, expected_leader);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_add_transaction_increases_pending_txs() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
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
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 0);

        for _ in 0..5 {
            manager.add_transaction(create_test_transaction());
        }

        assert_eq!(manager.pending_txs.len(), 5);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_block_proposal_for_current_view_returns_should_vote() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let parent_hash = [0; blake3::OUT_LEN];
        let block = create_test_block(0, leader_id, parent_hash, 1);

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

        // Current view is 0, propose for view 5
        let leader_id = setup.peer_set.sorted_peer_ids[1]; // View 5 % 4 = 1
        let parent_hash = [0; blake3::OUT_LEN];
        let block = create_test_block(5, leader_id, parent_hash, 1);

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

        // View 0 should have leader at index 0, but we use index 1
        let wrong_leader = setup.peer_set.sorted_peer_ids[1];
        let parent_hash = [0; blake3::OUT_LEN];
        let block = create_test_block(0, wrong_leader, parent_hash, 1);

        let result = manager.handle_block_proposal(block);
        assert!(result.is_err());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_block_proposal_with_future_view_and_wrong_leader_returns_error() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // View 5 should have leader at index 1, but we use index 0
        let wrong_leader = setup.peer_set.sorted_peer_ids[0];
        let parent_hash = [0; blake3::OUT_LEN];
        let block = create_test_block(5, wrong_leader, parent_hash, 1);

        let result = manager.handle_block_proposal(block);
        assert!(result.is_err());

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_new_vote_for_current_view_without_block_hash_returns_await_event() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let block_hash = [1u8; blake3::OUT_LEN];
        let vote = create_test_vote(2, 0, block_hash, leader_id, &setup);

        let result = manager.handle_new_vote(vote);
        assert!(result.is_ok());

        // Should await because no block has been received yet
        match result.unwrap() {
            ViewProgressEvent::Await => {}
            _ => panic!("Expected Await event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_new_vote_for_current_view_with_block_hash_returns_noop_event() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let block = create_test_block(0, leader_id, [0; blake3::OUT_LEN], 1);
        let block_hash = block.get_hash();

        manager.handle_block_proposal(block).unwrap();
        let vote = create_test_vote(2, 0, block_hash, leader_id, &setup);

        let result = manager.handle_new_vote(vote);
        assert!(result.is_ok());

        // Should await because no block has been received yet
        match result.unwrap() {
            ViewProgressEvent::NoOp => {}
            _ => panic!("Expected Await event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_new_vote_for_future_view_returns_should_update_view() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[1]; // Leader for view 5
        let block_hash = [1u8; blake3::OUT_LEN];
        let vote = create_test_vote(2, 5, block_hash, leader_id, &setup);

        let result = manager.handle_new_vote(vote);
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
    fn test_handle_new_vote_with_wrong_leader_returns_error() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let wrong_leader = setup.peer_set.sorted_peer_ids[2];
        let block_hash = [1u8; blake3::OUT_LEN];
        let vote = create_test_vote(2, 5, block_hash, wrong_leader, &setup);

        let result = manager.handle_new_vote(vote);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("is not the correct view leader")
        );

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_new_vote_after_block_triggers_m_notarization() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let parent_hash = [0; blake3::OUT_LEN];
        let block = create_test_block(0, leader_id, parent_hash, 1);
        let block_hash = block.get_hash();

        // First, handle the block proposal
        manager.handle_block_proposal(block).unwrap();

        // Add votes until m-notarization threshold (> 2*F = 2, so need 3)
        for i in 1..=3 {
            let vote = create_test_vote(i, 0, block_hash, leader_id, &setup);
            let result = manager.handle_new_vote(vote);
            assert!(result.is_ok());

            if i == 3 {
                match result.unwrap() {
                    ViewProgressEvent::ShouldMNotarize {
                        view,
                        block_hash: _,
                    } => {
                        assert_eq!(view, 0);
                    }
                    _ => panic!("Expected ShouldMNotarize event"),
                }
            } else {
                match result.unwrap() {
                    ViewProgressEvent::NoOp => {}
                    _ => panic!("Expected NoOp event"),
                }
            }
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

        // Only 1 nullify, need > 2*F = 2
        match result.unwrap() {
            ViewProgressEvent::NoOp => {}
            _ => panic!("Expected NoOp event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_nullify_for_future_view_returns_should_update_view() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[1]; // Leader for view 5
        let nullify = create_test_nullify(2, 5, leader_id, &setup);

        let result = manager.handle_nullify(nullify);
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
    fn test_handle_nullify_triggers_broadcast_when_threshold_reached() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];

        // Add nullify messages until threshold (> 2*F = 2, so need 3)
        for i in 1..=3 {
            let nullify = create_test_nullify(i, 0, leader_id, &setup);
            let result = manager.handle_nullify(nullify);
            assert!(result.is_ok());

            if i == 3 {
                match result.unwrap() {
                    ViewProgressEvent::ShouldBroadcastNullification { view } => {
                        assert_eq!(view, 0);
                    }
                    _ => panic!("Expected ShouldBroadcastNullification event"),
                }
            } else {
                match result.unwrap() {
                    ViewProgressEvent::NoOp => {}
                    _ => panic!("Expected NoOp event"),
                }
            }
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_m_notarization_for_future_view_returns_should_update_view() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[5]; // Leader for view 5
        let block_hash = [1u8; blake3::OUT_LEN];

        // Create m-notarization for future view
        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 5, block_hash, leader_id, &setup);
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 5, block_hash, leader_id);

        let result = manager.handle_m_notarization(m_notarization);
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
    fn test_handle_m_notarization_triggers_view_progress() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let parent_hash = [0; blake3::OUT_LEN];
        let block = create_test_block(0, leader_id, parent_hash, 1);
        let block_hash = block.get_hash();

        // First, handle the block proposal
        manager.handle_block_proposal(block).unwrap();

        // Create and handle m-notarization
        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 0, block_hash, leader_id, &setup);
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 0, block_hash, leader_id);

        let result = manager.handle_m_notarization(m_notarization);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ProgressToNextView {
                new_view,
                leader: _,
            } => {
                assert_eq!(new_view, 1);
            }
            _ => panic!("Expected ProgressToNextView event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_nullification_for_future_view_returns_should_update_view() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[5]; // Leader for view 5

        // Create nullification for future view
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            let nullify = create_test_nullify(i, 5, leader_id, &setup);
            nullify_messages.insert(nullify);
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 5, leader_id);

        let result = manager.handle_nullification(nullification);
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
    fn test_handle_nullification_for_current_view_returns_should_broadcast() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];

        // Create nullification for current view
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            let nullify = create_test_nullify(i, 0, leader_id, &setup);
            nullify_messages.insert(nullify);
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 0, leader_id);

        let result = manager.handle_nullification(nullification);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldBroadcastNullification { view } => {
                assert_eq!(view, 0);
            }
            _ => panic!("Expected ShouldBroadcastNullification event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_process_consensus_msg_block_proposal() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let parent_hash = [0; blake3::OUT_LEN];
        let block = create_test_block(0, leader_id, parent_hash, 1);
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
        let block_hash = [1u8; blake3::OUT_LEN];
        let vote = create_test_vote(2, 0, block_hash, leader_id, &setup);
        let msg = ConsensusMessage::Vote(vote);

        let result = manager.process_consensus_msg(msg);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::Await => {}
            _ => panic!("Expected Await event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_process_consensus_msg_nullify() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let nullify = create_test_nullify(2, 0, leader_id, &setup);
        let msg = ConsensusMessage::Nullify(nullify);

        let result = manager.process_consensus_msg(msg);
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::NoOp => {}
            _ => panic!("Expected NoOp event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_when_non_leader_and_has_block_should_vote() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let parent_hash = [0; blake3::OUT_LEN];
        let block = create_test_block(0, leader_id, parent_hash, 1);

        // Add block to context
        manager.current_view_context.block = Some(block.clone());
        manager.current_view_context.has_voted = false;

        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::ShouldVote {
                view,
                block_hash: _,
            } => {
                assert_eq!(view, 0);
                assert!(manager.current_view_context.has_voted);
            }
            _ => panic!("Expected ShouldVote event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_when_already_voted_returns_no_op() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        manager.current_view_context.has_voted = true;

        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::NoOp => {}
            _ => panic!("Expected NoOp event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_when_already_nullified_returns_no_op() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        manager.current_view_context.has_nullified = true;

        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::NoOp => {}
            _ => panic!("Expected NoOp event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_tick_without_block_returns_await() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        manager.current_view_context.has_voted = false;
        manager.current_view_context.has_nullified = false;
        manager.current_view_context.block = None;

        let result = manager.tick();
        assert!(result.is_ok());

        match result.unwrap() {
            ViewProgressEvent::Await => {}
            _ => panic!("Expected Await event"),
        }

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_vote_for_old_view_returns_error() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        // Manually advance to view 5
        manager.current_view_context.view_number = 5;

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let block_hash = [1u8; blake3::OUT_LEN];
        // Try to submit vote for old view 3
        let vote = create_test_vote(2, 3, block_hash, leader_id, &setup);

        let result = manager.handle_new_vote(vote);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("is not the current view")
        );

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_multiple_block_proposals_for_same_view_returns_error() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let parent_hash = [0; blake3::OUT_LEN];

        let block1 = create_test_block(0, leader_id, parent_hash, 1);
        let block2 = create_test_block(0, leader_id, parent_hash, 2);

        // First block should succeed
        let result1 = manager.handle_block_proposal(block1);
        assert!(result1.is_ok());

        // Second block for same view should fail
        let result2 = manager.handle_block_proposal(block2);
        assert!(result2.is_err());
        assert!(result2.unwrap_err().to_string().contains("already exists"));

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_handle_block_proposal_with_wrong_parent_hash_returns_error() {
        let setup = create_test_peer_setup(4);
        let (mut manager, path): (ViewProgressManager<4, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let wrong_parent_hash = [99u8; blake3::OUT_LEN];
        let block = create_test_block(0, leader_id, wrong_parent_hash, 1);

        let result = manager.handle_block_proposal(block);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("parent block hash")
        );

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_concurrent_votes_and_block_proposal() {
        let setup = create_test_peer_setup(6);
        let (mut manager, path): (ViewProgressManager<6, 1, 3>, String) =
            create_test_manager(&setup, 1);

        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let parent_hash = [0; blake3::OUT_LEN];
        let block = create_test_block(0, leader_id, parent_hash, 1);
        let block_hash = block.get_hash();

        // Add votes before block (should go to non-verified)
        let vote1 = create_test_vote(2, 0, block_hash, leader_id, &setup);
        let vote2 = create_test_vote(3, 0, block_hash, leader_id, &setup);

        manager.handle_new_vote(vote1).unwrap();
        manager.handle_new_vote(vote2).unwrap();

        // Now add the block (should move non-verified votes to verified)
        let result = manager.handle_block_proposal(block);
        assert!(result.is_ok());

        // Votes should have been moved to verified set
        assert_eq!(manager.current_view_context.votes.len(), 2);
        assert_eq!(manager.current_view_context.non_verified_votes.len(), 0);

        std::fs::remove_file(path).unwrap();
    }
}
