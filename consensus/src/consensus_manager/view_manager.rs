use std::str::FromStr;

use anyhow::Result;
use tracing::instrument;

use crate::{
    consensus::ConsensusMessage,
    consensus_manager::{
        config::ConsensusConfig,
        events::ViewProgressEvent,
        leader_manager::{LeaderManager, LeaderSelectionStrategy, RoundRobinLeaderManager},
        view_context::{
            CollectedNullificationsResult, CollectedVotesResult, LeaderProposalResult,
            ShouldMNotarize, ViewContext,
        },
    },
    crypto::aggregated::{BlsPublicKey, PeerId},
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

    /// The persistence storage for the consensus protocol
    ///
    /// This is used to persist the view contexts and the votes/nullifications/notarizations
    /// whenever a view in the [`ViewChain`] is finalized by the state machine replication
    /// protocol.
    _persistence_storage: ConsensusStore,

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
        Ok(Self {
            config,
            leader_manager,
            current_view_context: view_context,
            unfinalized_view_context: None,
            _persistence_storage: persistence_storage,
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
        Ok(Self {
            config,
            leader_manager,
            current_view_context: view_context,
            unfinalized_view_context: None,
            peers,
            pending_txs: Vec::new(),
            _persistence_storage: persistence_storage,
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
            // In this case, the replica has either voted for a block, or nullified the current
            // view. In both cases, we can return a no-op event.
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

    // pub fn update_view(&mut self) -> Result<()> {
    //     let new_leader = self
    //         .leader_manager
    //         .leader_for_view(self.current_view_context.view_number + 1)?;

    //     if self.current_view_context.nullification.is_none()
    //         && self.current_view_context.m_notarization.is_none()
    //     {
    //         return Err(anyhow::anyhow!(
    //             "Cannot update current view to a new view without a nullification or
    // m-notarization"         ));
    //     }

    //     if let Some(ref nullification) = self.current_view_context.nullification {
    //         // Persist the nullified view to the persistence storage.
    //         // self.persist_nullified_view(&self.current_view_context)?;

    //         // Create a new view context for the next view.
    //         let new_view_context = ViewContext::<N, F, M_SIZE>::new(
    //             nullification.view + 1,
    //             new_leader.peer_id(),
    //             self.current_view_context.replica_id,
    //             self.current_view_context.parent_block_hash, // NOTE: No progress was made, use
    // same parent         );
    //         self.unfinalized_view_context = Some(self.current_view_context);

    //         // Clear unfinalized view context on nullification
    //         // (The nullified view is persisted and won't be finalized)
    //         self.unfinalized_view_context = None;

    //         // Progress to the next view (v := v + 1, reset state)
    //         self.current_view_context = new_view_context;

    //         return Ok(());
    //     }

    //     if let Some(ref current_view_m_notarization) = self.current_view_context.m_notarization {
    //         // NOTE: This indicates that the view change has been triggered by a m-notarization.
    //         // Therefore, the next view can consider the current view block hash as its parent
    // block hash.         let new_view_context = ViewContext::<N, F, M_SIZE>::new(
    //             current_view_m_notarization.view + 1,
    //             new_leader.peer_id(),
    //             self.current_view_context.replica_id,
    //             current_view_m_notarization.block_hash,
    //         );
    //         self.unfinalized_view_context = Some(self.current_view_context);
    //         self.current_view_context = new_view_context;

    //         return Ok(());
    //     }

    //     Ok(())
    // }

    /// [`handle_block_proposal`] is called when the current replica receives a new block proposal,
    /// from the leader of the current view.
    ///
    /// It validates the block and adds it to the current view context.
    /// If the block is for a future view, it returns a [`ViewProgressEvent::ShouldUpdateView`]
    /// event, to update the view context to the future view.
    /// If the block is for the current view, and it passes all validation checks in
    /// [`ViewContext::add_new_view_block`], then the method returns a
    /// [`ViewProgressEvent::ShouldVote`] event, to vote for the block.
    fn handle_block_proposal(&mut self, block: Block) -> Result<ViewProgressEvent<N, F, M_SIZE>> {
        // Validate block for the current view
        if block.header.view > self.current_view_context.view_number {
            // If the block is for a future view, then we need to update the view context to the
            // future view, if the leader of the future view is not the current leader.
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
            should_await,
            should_vote,
        } = self.current_view_context.add_new_view_block(block)?;

        if is_enough_to_m_notarize {
            // NOTE: In this case, the replica has collected enough votes to propose a
            // M-notarization, but not enough to finalize the view. Therefore, the
            // replica should vote for the block and notarize it simultaneously.
            return Ok(ViewProgressEvent::ShouldVoteAndMNotarize {
                view: self.current_view_context.view_number,
                block_hash,
            });
        } else if is_enough_to_finalize {
            // NOTE: In this case, the replica has collected enough votes to finalize the view,
            // before it has received the block proposal from the leader, as most likely the replica
            // was beyond. In such case, the replica should vote for the block and finalize the view
            // simultaneously.
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
            // but that involves more work, as it would require a supra-majority of replicas
            // providing more blocks to the current replica.
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
                should_nullify,
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
                    should_nullify,
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
                should_vote,
                should_nullify,
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
                // NOTE: There is not anything left to do here, as the m-notarization has already
                // been added to the unfinalized view context.
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
