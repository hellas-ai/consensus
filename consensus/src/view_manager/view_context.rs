use std::{collections::HashSet, time::Instant};

use anyhow::Result;

use crate::{
    crypto::aggregated::PeerId,
    state::{
        block::Block,
        notarizations::{MNotarization, Vote},
        nullify::{Nullification, Nullify},
        peer::PeerSet,
    },
    view_manager::utils::{
        NotarizationData, NullificationData, create_notarization_data, create_nullification_data,
    },
};

/// Per-view state tracking
#[derive(Debug, Clone)]
pub struct ViewContext<const N: usize, const F: usize, const M_SIZE: usize> {
    /// The view number
    pub view_number: u64,

    /// When the current replica entered this view
    pub entered_at: Instant,

    /// Whether the current replica has voted in this view
    pub has_voted: bool,

    /// The block the current replica voted for (if any)
    pub block_hash: Option<[u8; blake3::OUT_LEN]>,

    /// The parent block hash of the current view's block
    pub parent_block_hash: [u8; blake3::OUT_LEN],

    /// Whether the current replica has sent a nullify message
    pub has_nullified: bool,

    /// Whether the current replica has proposed a block (if it is the leader)
    pub has_proposed: bool,

    /// The leader's ID of the view
    pub leader_id: PeerId,

    /// The replica's own peer ID
    pub replica_id: PeerId,

    /// The block received by the current view's leader for the current view (if any).
    pub block: Option<Block>,

    /// Received votes for the current view's block
    pub votes: HashSet<Vote>,

    /// Non-verified votes for the current view's block
    pub non_verified_votes: HashSet<Vote>,

    /// A m-notarization for the current view (if any)
    pub m_notarization: Option<MNotarization<N, F, M_SIZE>>,

    /// A non-verified m-notarization for the current view (if any)
    pub non_verified_m_notarization: Option<MNotarization<N, F, M_SIZE>>,

    /// Received nullify messages for the current view
    pub nullify_messages: HashSet<Nullify>,

    /// A nullification for the current view (if any)
    pub nullification: Option<Nullification<N, F, M_SIZE>>,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> ViewContext<N, F, M_SIZE> {
    pub fn new(
        view_number: u64,
        leader_id: PeerId,
        replica_id: PeerId,
        parent_block_hash: [u8; blake3::OUT_LEN],
    ) -> Self {
        Self {
            view_number,
            block: None,
            votes: HashSet::new(),
            replica_id,
            m_notarization: None,
            non_verified_m_notarization: None,
            nullification: None,
            nullify_messages: HashSet::new(),
            entered_at: Instant::now(),
            non_verified_votes: HashSet::new(),
            has_voted: false,
            block_hash: None,
            parent_block_hash,
            has_nullified: false,
            has_proposed: false,
            leader_id,
        }
    }

    /// Adds a proposed block to the current view's context.
    ///
    /// Validates the block's view number, leader, and parent hash match the current view.
    /// Sets the block hash and moves any non-verified votes to the verified set.
    /// Returns whether enough votes exist for M-notarization or finalization.
    ///
    /// Returns an error if the block is invalid.
    pub fn add_new_view_block(&mut self, block: Block) -> Result<LeaderProposalResult> {
        if block.view() != self.view_number {
            return Err(anyhow::anyhow!(
                "Proposed block for view {} is not the current view {}",
                block.view(),
                self.view_number
            ));
        }

        if self.block.is_some() {
            return Err(anyhow::anyhow!(
                "Block for view {} already exists",
                self.view_number
            ));
        }

        if block.leader != self.leader_id {
            return Err(anyhow::anyhow!(
                "Proposed block for leader {} is not the current leader {}",
                block.leader,
                self.leader_id
            ));
        }

        if block.parent_block_hash() != self.parent_block_hash {
            return Err(anyhow::anyhow!(
                "Proposed block for parent block hash {} is not the current parent block hash {}",
                hex::encode(block.parent_block_hash()),
                hex::encode(self.parent_block_hash)
            ));
        }

        let block_hash = block.get_hash();

        self.block_hash = Some(block_hash);
        self.block = Some(block);
        // TODO: We need to add the leader's block proposal as a vote to the votes set.

        if !self.non_verified_votes.is_empty() {
            self.votes.extend(self.non_verified_votes.drain());
        }

        if self.non_verified_m_notarization.is_some() {
            // NOTE: We need to verify that the non-verified m-notarization is for the same block as the proposed block.
            if self
                .non_verified_m_notarization
                .as_ref()
                .unwrap()
                .block_hash
                == block_hash
            {
                self.m_notarization = self.non_verified_m_notarization.take();
            } else {
                // NOTE: The non-verified m-notarization is for a different block, so we can safely ignore it.
                self.non_verified_m_notarization = None;
            }
        }

        let is_enough_to_m_notarize = self.votes.len() > 2 * F || self.m_notarization.is_some();
        let is_enough_to_finalize = self.votes.len() > N - F;

        Ok(LeaderProposalResult {
            block_hash,
            is_enough_to_m_notarize,
            is_enough_to_finalize,
        })
    }

    /// Adds a vote to the current view's context.
    ///
    /// Validates peer membership, view number, signature, and prevents duplicates.
    /// Stores vote in verified set if block hash matches, otherwise in non-verified set.
    /// Returns whether enough votes exist for M-notarization or finalization.
    ///
    /// Returns an error if validation fails.
    pub fn add_vote(&mut self, vote: Vote, peers: &PeerSet) -> Result<CollectedVotesResult> {
        if !peers.contains(&vote.peer_id) {
            return Err(anyhow::anyhow!(
                "Vote for peer {} is not present in the peers set",
                vote.peer_id
            ));
        }

        if vote.view != self.view_number {
            return Err(anyhow::anyhow!(
                "Vote for view {} is not the current view {}",
                vote.view,
                self.view_number
            ));
        }

        if self.votes.iter().any(|v| v.peer_id == vote.peer_id) {
            return Err(anyhow::anyhow!(
                "Vote for peer {} already exists",
                vote.peer_id
            ));
        }

        if self
            .non_verified_votes
            .iter()
            .any(|v| v.peer_id == vote.peer_id)
        {
            return Err(anyhow::anyhow!(
                "Vote for peer {} already exists in the non-verified votes set",
                vote.peer_id
            ));
        }

        let peer_public_key = peers.get_public_key(&vote.peer_id)?;
        if !vote.verify(peer_public_key) {
            return Err(anyhow::anyhow!(
                "Vote signature is not valid for peer {}",
                vote.peer_id
            ));
        }

        if self.block_hash.is_none() {
            // NOTE: In this case, the replica has not yet received the view proposed block hash
            // from the leader, so we need to store the vote in the non-verified votes set.
            self.non_verified_votes.insert(vote);
            return Ok(CollectedVotesResult {
                should_await: true,
                is_enough_to_m_notarize: false,
                is_enough_to_finalize: false,
            });
        }

        let block_hash = self.block_hash.unwrap();

        if vote.block_hash != block_hash {
            return Err(anyhow::anyhow!(
                "Vote for block hash {} is not the block hash for the current view {}",
                hex::encode(vote.block_hash),
                hex::encode(block_hash)
            ));
        }

        self.votes.insert(vote);

        let is_enough_to_m_notarize = self.votes.len() > 2 * F;
        let is_enough_to_finalize = self.votes.len() >= N - F;

        if is_enough_to_m_notarize {
            let NotarizationData {
                peer_ids,
                aggregated_signature,
            } = create_notarization_data::<M_SIZE>(&self.votes)?;
            self.m_notarization = Some(MNotarization::new(
                self.view_number,
                block_hash,
                aggregated_signature,
                peer_ids,
                self.leader_id,
            ));
        }

        Ok(CollectedVotesResult {
            should_await: false,
            is_enough_to_m_notarize,
            is_enough_to_finalize,
        })
    }

    /// Adds a nullify message to the current view's context.
    ///
    /// Validates peer membership, view number, leader ID, signature, and prevents duplicates.
    /// Creates a nullification if enough nullify messages are collected.
    ///
    /// Returns an error if validation fails.
    pub fn add_nullify(&mut self, nullify: Nullify, peers: &PeerSet) -> Result<()> {
        if !peers.contains(&nullify.peer_id) {
            return Err(anyhow::anyhow!(
                "Nullify for peer {} is not present in the peers set",
                nullify.peer_id
            ));
        }

        if nullify.view != self.view_number {
            return Err(anyhow::anyhow!(
                "Nullify for view {} is not the current view {}",
                nullify.view,
                self.view_number
            ));
        }

        if nullify.leader_id != self.leader_id {
            return Err(anyhow::anyhow!(
                "Nullify for leader {} is not the current leader {}",
                nullify.leader_id,
                self.leader_id
            ));
        }

        if self
            .nullify_messages
            .iter()
            .any(|n| n.peer_id == nullify.peer_id)
        {
            return Err(anyhow::anyhow!(
                "Nullify for peer {} already exists",
                nullify.peer_id
            ));
        }

        let peer_public_key = peers.get_public_key(&nullify.peer_id)?;
        if !nullify.verify(peer_public_key) {
            return Err(anyhow::anyhow!(
                "Nullify signature is not valid for peer {}",
                nullify.peer_id
            ));
        }

        let is_enough_for_nullification = self.nullify_messages.len() > 2 * F;

        self.nullify_messages.insert(nullify);

        let nullification = if is_enough_for_nullification {
            let NullificationData {
                peer_ids,
                aggregated_signature,
            } = create_nullification_data::<M_SIZE>(&self.nullify_messages)?;
            Some(Nullification::new(
                self.view_number,
                self.leader_id,
                aggregated_signature,
                peer_ids,
            ))
        } else {
            None
        };

        self.nullification = nullification;

        Ok(())
    }

    /// Adds a M-notarization to the current view's context.
    ///
    /// Validates view number, leader ID, and signature.
    /// Stores as verified if block hash matches, otherwise as non-verified.
    /// Returns whether the replica should M-notarize or await.
    ///
    /// Returns an error if validation fails.
    pub fn add_m_notarization(
        &mut self,
        m_notarization: MNotarization<N, F, M_SIZE>,
        peers: &PeerSet,
    ) -> Result<ShouldMNotarize> {
        if m_notarization.view != self.view_number {
            return Err(anyhow::anyhow!(
                "M-notarization for view {} is not the current view {}",
                m_notarization.view,
                self.view_number
            ));
        }

        if m_notarization.leader_id != self.leader_id {
            return Err(anyhow::anyhow!(
                "M-notarization for leader {} is not the current leader {}",
                m_notarization.leader_id,
                self.leader_id
            ));
        }

        if !m_notarization.verify(peers) {
            return Err(anyhow::anyhow!(
                "M-notarization signature is not valid for the current view",
            ));
        }

        if self.block_hash.is_none() {
            if self.m_notarization.is_none() {
                self.non_verified_m_notarization = Some(m_notarization);
            }
            return Ok(ShouldMNotarize {
                should_notarize: false,
                should_await: true,
            });
        }

        let block_hash = self.block_hash.unwrap();
        if m_notarization.block_hash != block_hash {
            return Err(anyhow::anyhow!(
                "M-notarization for block hash {} is not the block hash for the current view {}",
                hex::encode(m_notarization.block_hash),
                hex::encode(block_hash)
            ));
        }

        if self.m_notarization.is_some() {
            // We already have a m-notarization for the current view, so we can safely ignore this one.
            return Ok(ShouldMNotarize {
                should_notarize: false,
                should_await: false,
            });
        }

        self.m_notarization = Some(m_notarization);

        Ok(ShouldMNotarize {
            should_notarize: true,
            should_await: false,
        })
    }

    /// Adds a nullification to the current view's context.
    ///
    /// Validates view number, leader ID, and signature.
    /// Stores the nullification if not already present.
    /// Returns whether the replica should broadcast the nullification.
    ///
    /// Returns an error if validation fails.
    pub fn add_nullification(
        &mut self,
        nullification: Nullification<N, F, M_SIZE>,
        peers: &PeerSet,
    ) -> Result<CollectedNullificationsResult> {
        if nullification.view != self.view_number {
            return Err(anyhow::anyhow!(
                "Nullification for view {} is not the current view {}",
                nullification.view,
                self.view_number
            ));
        }

        if nullification.leader_id != self.leader_id {
            return Err(anyhow::anyhow!(
                "Nullification for leader {} is not the current leader {}",
                nullification.leader_id,
                self.leader_id
            ));
        }

        if !nullification.verify(peers) {
            return Err(anyhow::anyhow!(
                "Nullification signature is not valid for the current view",
            ));
        }

        if self.nullification.is_some() {
            // NOTE: We already have a nullification for the current view, so we can safely ignore this one.
            return Ok(CollectedNullificationsResult {
                should_broadcast_nullification: false,
            });
        }

        self.nullification = Some(nullification);

        Ok(CollectedNullificationsResult {
            should_broadcast_nullification: true,
        })
    }

    /// [`is_leader`] checks if the current replica is the leader for the current view.
    #[inline]
    pub fn is_leader(&self) -> bool {
        self.leader_id == self.replica_id
    }

    /// [`has_view_progressed_without_m_notarization`] checks if the current view has progressed without a m-notarization.
    /// It returns an error if the current m-notarization is not present. This method is explicitly called by the view progress manager
    /// to ensure the `unfinalized_view_context` is not left in an invalid state.
    #[inline]
    pub fn has_view_progressed_without_m_notarization(&self) -> Result<()> {
        if self.m_notarization.is_none() {
            return Err(anyhow::anyhow!(
                "The current m-notarization for the current view {} is not yet present, but the view has already progressed without a m-notarization, that should never happen.",
                self.view_number
            ));
        }
        Ok(())
    }
}

/// [`LeaderProposalResult`] is the result of receiving a new view leader's block proposal.
/// In the unlikely case that a replica receives a block proposal for the current view, after collecting enough votes,
/// we allow it to step the state machine with either a m-notarization or a finalization (if enough votes have been collected).
#[derive(Debug)]
pub struct LeaderProposalResult {
    /// The hash of the block that the replica should vote for
    pub block_hash: [u8; blake3::OUT_LEN],
    /// Whether the current replica has collected enough votes to propose a M-notarization
    pub is_enough_to_m_notarize: bool,
    /// Whether the current replica has collected enough votes to finalize the view
    pub is_enough_to_finalize: bool,
}

/// [`CollectedVotesResult`] is the result of collecting votes for the current view's block.
/// It is used to determine if the current replica should propose a M-notarization or finalize the view.
#[derive(Debug)]
pub struct CollectedVotesResult {
    /// Whether the current replica should await for the current view's leader to propose a block
    pub should_await: bool,
    /// Whether the current replica has collected enough votes to propose a M-notarization
    pub is_enough_to_m_notarize: bool,
    /// Whether the current replica has collected enough votes to finalize the view
    pub is_enough_to_finalize: bool,
}

/// [`ShouldMNotarize`] is the result of processing a newly received m-notarization for the current view.
#[derive(Debug)]
pub struct ShouldMNotarize {
    /// Whether the current replica should notarize the current view's block
    pub should_notarize: bool,
    /// Whether the current replica should finalize the current view
    pub should_await: bool,
}

/// [`CollectedNullificationsResult`] is the result of collecting nullifications for the current view.
/// It is used to determine if the current replica should broadcast a nullification.
#[derive(Debug)]
pub struct CollectedNullificationsResult {
    /// Whether the current replica has collected enough nullifications to broadcast a nullification
    pub should_broadcast_nullification: bool,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::{
        crypto::aggregated::BlsSecretKey,
        state::{block::Block, transaction::Transaction},
    };
    use rand::thread_rng;

    // Helper function to generate a test transaction
    fn gen_tx() -> Transaction {
        let mut rng = thread_rng();
        let sk = BlsSecretKey::generate(&mut rng);
        let pk = sk.public_key();
        let tx_hash: [u8; blake3::OUT_LEN] = blake3::hash(b"test tx").into();
        let sig = sk.sign(&tx_hash);
        Transaction::new(pk, [7u8; 32], 42, 9, 1_000, 3, tx_hash, sig)
    }

    // Helper function to create a test block
    fn create_test_block(
        view: u64,
        leader: PeerId,
        parent_hash: [u8; blake3::OUT_LEN],
        height: u64,
    ) -> Block {
        let transactions = vec![gen_tx()];
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

    // Helper struct to hold both peer set and secret keys for testing
    struct TestPeerSetup {
        peer_set: PeerSet,
        peer_id_to_secret_key: HashMap<PeerId, BlsSecretKey>,
    }

    // Helper function to create a test peer set with secret keys
    fn create_test_peer_setup(size: usize) -> TestPeerSetup {
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

        TestPeerSetup {
            peer_set: PeerSet::new(public_keys),
            peer_id_to_secret_key,
        }
    }

    // Helper function to create a signed vote from a peer
    fn create_test_vote(
        peer_index: usize,
        view: u64,
        block_hash: [u8; blake3::OUT_LEN],
        leader_id: PeerId,
        setup: &TestPeerSetup,
    ) -> Vote {
        let peer_id = setup.peer_set.sorted_peer_ids[peer_index];
        let secret_key = setup.peer_id_to_secret_key.get(&peer_id).unwrap();
        let signature = secret_key.sign(&block_hash);

        Vote::new(view, block_hash, signature, peer_id, leader_id)
    }

    // Helper function to create a test view context
    fn create_test_view_context(
        view_number: u64,
        leader_id: PeerId,
        replica_id: PeerId,
        parent_block_hash: [u8; blake3::OUT_LEN],
    ) -> ViewContext<4, 1, 96> {
        ViewContext::new(view_number, leader_id, replica_id, parent_block_hash)
    }

    /// Helper function to create a test view context with custom parameters
    fn creat_test_view_context_with_params<const N: usize, const F: usize, const M_SIZE: usize>(
        view_number: u64,
        leader_id: PeerId,
        replica_id: PeerId,
        parent_block_hash: [u8; blake3::OUT_LEN],
    ) -> ViewContext<N, F, M_SIZE> {
        ViewContext::new(view_number, leader_id, replica_id, parent_block_hash)
    }

    #[test]
    fn test_add_new_view_block_success() {
        let setup = create_test_peer_setup(4);
        let peers = setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [1u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(5, leader_id, replica_id, parent_hash);

        let block = create_test_block(5, leader_id, parent_hash, 1);
        let expected_hash = block.get_hash();

        let result = context.add_new_view_block(block.clone());

        assert!(result.is_ok());
        let block_hash = result.unwrap().block_hash;
        assert_eq!(block_hash, expected_hash);
        assert_eq!(context.block_hash, Some(expected_hash));
        assert_eq!(context.block, Some(block));
        assert!(!context.has_voted);
        assert!(!context.has_nullified);
        assert!(!context.has_proposed);
    }

    #[test]
    fn test_add_new_view_block_moves_non_verified_votes() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [2u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Add a non-verified vote before adding the block

        let vote = create_test_vote(1, 10, [9u8; blake3::OUT_LEN], leader_id, &setup);
        context.non_verified_votes.insert(vote.clone());

        assert_eq!(context.non_verified_votes.len(), 1);
        assert_eq!(context.votes.len(), 0);

        let block = create_test_block(10, leader_id, parent_hash, 2);
        let _block_hash = block.get_hash();

        let result = context.add_new_view_block(block);

        assert!(result.is_ok());
        assert_eq!(context.votes.len(), 1);
        assert_eq!(context.non_verified_votes.len(), 0);
        assert!(context.votes.contains(&vote));
    }

    #[test]
    fn test_add_new_view_block_wrong_view() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [3u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(15, leader_id, replica_id, parent_hash);

        // Create block with wrong view number
        let block = create_test_block(20, leader_id, parent_hash, 3);

        let result = context.add_new_view_block(block);

        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("Proposed block for view 20 is not the current view 15"));
        assert!(context.block.is_none());
        assert!(context.block_hash.is_none());
    }

    #[test]
    fn test_add_new_view_block_already_exists() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [4u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(8, leader_id, replica_id, parent_hash);

        // Add first block
        let block1 = create_test_block(8, leader_id, parent_hash, 4);
        let result1 = context.add_new_view_block(block1);
        assert!(result1.is_ok());

        // Try to add second block
        let block2 = create_test_block(8, leader_id, parent_hash, 5);
        let result2 = context.add_new_view_block(block2);

        assert!(result2.is_err());
        let error_msg = result2.unwrap_err().to_string();
        assert!(error_msg.contains("Block for view 8 already exists"));
        // Original block should still be there
        assert!(context.block.is_some());
        assert!(context.block_hash.is_some());
    }

    #[test]
    fn test_add_new_view_block_wrong_leader() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let correct_leader = peers.sorted_peer_ids[0];
        let wrong_leader = peers.sorted_peer_ids[1];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [5u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(12, correct_leader, replica_id, parent_hash);

        // Create block with wrong leader
        let block = create_test_block(12, wrong_leader, parent_hash, 6);

        let result = context.add_new_view_block(block);

        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains(&format!(
            "Proposed block for leader {} is not the current leader {}",
            wrong_leader, correct_leader
        )));
        assert!(context.block.is_none());
        assert!(context.block_hash.is_none());
    }

    #[test]
    fn test_add_new_view_block_wrong_parent_hash() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let correct_parent = [6u8; blake3::OUT_LEN];
        let wrong_parent = [7u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(18, leader_id, replica_id, correct_parent);

        // Create block with wrong parent hash
        let block = create_test_block(18, leader_id, wrong_parent, 7);

        let result = context.add_new_view_block(block);

        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("Proposed block for parent block hash"));
        assert!(error_msg.contains("is not the current parent block hash"));
        assert!(context.block.is_none());
        assert!(context.block_hash.is_none());
    }

    #[test]
    fn test_add_new_view_block_preserves_other_state() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [8u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(25, leader_id, replica_id, parent_hash);

        // Set some initial state
        context.has_voted = true;
        context.has_nullified = true;
        context.has_proposed = true;
        context.view_number = 25;
        context.leader_id = leader_id;

        let block = create_test_block(25, leader_id, parent_hash, 8);
        let result = context.add_new_view_block(block);

        assert!(result.is_ok());
        // These flags should remain unchanged
        assert!(context.has_voted);
        assert!(context.has_nullified);
        assert!(context.has_proposed);
        assert_eq!(context.view_number, 25);
        assert_eq!(context.leader_id, leader_id);
        // Only block-related fields should change
        assert!(context.block.is_some());
        assert!(context.block_hash.is_some());
    }

    #[test]
    fn test_add_vote_success() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [1u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Set a block hash first
        let block = create_test_block(10, leader_id, parent_hash, 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Create and add a valid vote
        let vote = create_test_vote(2, 10, block_hash, leader_id, &setup);
        let result = context.add_vote(vote.clone(), peers);

        assert!(result.is_ok());
        let vote_result = result.unwrap();
        assert!(!vote_result.should_await);
        assert!(!vote_result.is_enough_to_m_notarize); // Only 1 vote, need > 2*F = 2
        assert!(!vote_result.is_enough_to_finalize); // Only 1 vote, need > N-F = 3
        assert!(context.votes.contains(&vote));
        assert_eq!(context.votes.len(), 1);
    }

    #[test]
    fn test_add_vote_peer_not_in_set() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [2u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Create a vote with invalid peer ID (not in peer set)
        let invalid_peer_id = 10; // Random peer ID not in set
        let block_hash = [3u8; blake3::OUT_LEN];
        let secret_key = BlsSecretKey::generate(&mut thread_rng());
        let signature = secret_key.sign(&block_hash);
        let invalid_vote = Vote::new(10, block_hash, signature, invalid_peer_id, leader_id);

        let result = context.add_vote(invalid_vote, peers);
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("is not present in the peers set"));
    }

    #[test]
    fn test_add_vote_wrong_view() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [3u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Create vote with wrong view number
        let vote = create_test_vote(1, 15, [4u8; blake3::OUT_LEN], leader_id, &setup); // view 15 instead of 10
        let result = context.add_vote(vote, peers);

        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("Vote for view 15 is not the current view 10"));
    }

    #[test]
    fn test_add_vote_duplicate_in_verified_set() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [4u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Set a block hash first
        let block = create_test_block(10, leader_id, parent_hash, 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Add first vote
        let vote = create_test_vote(1, 10, block_hash, leader_id, &setup);
        let result1 = context.add_vote(vote.clone(), peers);
        assert!(result1.is_ok());

        // Try to add the same vote again
        let result2 = context.add_vote(vote, peers);
        assert!(result2.is_err());
        let error_msg = result2.unwrap_err().to_string();
        assert!(error_msg.contains("already exists"));
    }

    #[test]
    fn test_add_vote_duplicate_in_non_verified_set() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [5u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Add vote to non-verified set first (no block hash set)
        let vote = create_test_vote(1, 10, [6u8; blake3::OUT_LEN], leader_id, &setup);
        let result1 = context.add_vote(vote.clone(), peers);
        assert!(result1.is_ok());
        assert!(context.non_verified_votes.contains(&vote));

        // Try to add the same vote again
        let result2 = context.add_vote(vote, peers);
        assert!(result2.is_err());
        let error_msg = result2.unwrap_err().to_string();
        assert!(error_msg.contains("already exists in the non-verified votes set"));
    }

    #[test]
    fn test_add_vote_invalid_signature() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [7u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Set a block hash first
        let block = create_test_block(10, leader_id, parent_hash, 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Create vote with invalid signature (wrong secret key)
        let peer_id = peers.sorted_peer_ids[1];
        let wrong_secret_key = BlsSecretKey::generate(&mut thread_rng()); // Different key
        let invalid_signature = wrong_secret_key.sign(&block_hash);
        let invalid_vote = Vote::new(10, block_hash, invalid_signature, peer_id, leader_id);

        let result = context.add_vote(invalid_vote, peers);
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("signature is not valid"));
    }

    #[test]
    fn test_add_vote_stores_in_non_verified_when_no_block_hash() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [8u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Don't set block hash - should store in non-verified
        let vote = create_test_vote(2, 10, [9u8; blake3::OUT_LEN], leader_id, &setup);
        let result = context.add_vote(vote.clone(), peers);

        assert!(result.is_ok());
        let vote_result = result.unwrap();
        assert!(vote_result.should_await);
        assert!(!vote_result.is_enough_to_m_notarize);
        assert!(!vote_result.is_enough_to_finalize);
        assert!(context.non_verified_votes.contains(&vote));
        assert_eq!(context.votes.len(), 0);
    }

    #[test]
    fn test_add_vote_wrong_block_hash() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [10u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Set a block hash first
        let block = create_test_block(10, leader_id, parent_hash, 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Create vote with different block hash
        let wrong_block_hash = [11u8; blake3::OUT_LEN];
        let vote = create_test_vote(1, 10, wrong_block_hash, leader_id, &setup);
        let result = context.add_vote(vote, peers);

        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("is not the block hash for the current view"));
    }

    #[test]
    fn test_add_vote_creates_m_notarization_when_threshold_reached() {
        let setup = create_test_peer_setup(6); // N=6, F=1, so 2*F+1 = 3 votes needed for M-notarization
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [12u8; blake3::OUT_LEN];
        let mut context =
            creat_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Set a block hash first
        let block = create_test_block(10, leader_id, parent_hash, 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Add votes until we reach the threshold (2*F + 1 = 3)
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            let result = context.add_vote(vote, peers);
            assert!(result.is_ok());

            let vote_result = result.unwrap();
            if i < 3 {
                assert!(!vote_result.is_enough_to_m_notarize);
            } else {
                assert!(vote_result.is_enough_to_m_notarize);
            }
        }

        assert_eq!(context.votes.len(), 3);
        assert!(context.m_notarization.is_some());
        let notarization = context.m_notarization.as_ref().unwrap();
        assert_eq!(notarization.view, 10);
        assert_eq!(notarization.block_hash, block_hash);
        assert_eq!(notarization.leader_id, leader_id);
    }

    #[test]
    fn test_add_vote_enough_for_finalization() {
        let setup = create_test_peer_setup(6); // N=6, F=1, so N-F = 5 votes needed for finalization
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [13u8; blake3::OUT_LEN];
        let mut context =
            creat_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Set a block hash first
        let block = create_test_block(10, leader_id, parent_hash, 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Add votes until we reach finalization threshold (N-F = 3)
        for i in 1..=5 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            let result = context.add_vote(vote, peers);
            assert!(result.is_ok());

            let vote_result = result.unwrap();
            if i < 5 {
                assert!(!vote_result.is_enough_to_finalize);
            } else {
                assert!(vote_result.is_enough_to_finalize);
            }
        }

        assert_eq!(context.votes.len(), 5);
    }

    #[test]
    fn test_add_vote_moves_non_verified_to_verified() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [14u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Add a non-verified vote first
        let block = create_test_block(10, leader_id, parent_hash, 1);
        let block_hash = block.get_hash();
        let non_verified_vote = create_test_vote(1, 10, block_hash, leader_id, &setup);
        let result1 = context.add_vote(non_verified_vote.clone(), peers);
        assert!(result1.is_ok());
        assert!(context.non_verified_votes.contains(&non_verified_vote));

        // Now add the block with matching hash
        let _ = context.add_new_view_block(block).unwrap();
        // Manually set the block hash to match our vote
        context.block_hash = Some(block_hash);

        // Add another vote with the same block hash
        let verified_vote = create_test_vote(2, 10, block_hash, leader_id, &setup);
        let result2 = context.add_vote(verified_vote.clone(), peers);
        assert!(result2.is_ok());

        // Both votes should now be in the verified set
        assert!(!context.non_verified_votes.contains(&non_verified_vote));
        assert!(context.votes.contains(&verified_vote));
        assert!(context.votes.contains(&non_verified_vote));
        assert_eq!(context.votes.len(), 2);
        assert_eq!(context.non_verified_votes.len(), 0);
    }
}
