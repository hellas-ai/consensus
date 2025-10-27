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

    /// [`add_proposed_block`] adds a proposed block to the current view's context.
    /// It returns the hash of the proposed block.
    /// If the block is not valid, an error is returned instead.
    pub fn add_new_view_block(&mut self, block: Block) -> Result<[u8; blake3::OUT_LEN]> {
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
                hex::encode(&block.parent_block_hash()),
                hex::encode(&self.parent_block_hash)
            ));
        }

        let block_hash = block.get_hash();

        self.block_hash = Some(block_hash);
        self.block = Some(block);

        if !self.non_verified_votes.is_empty() {
            self.votes.extend(self.non_verified_votes.drain());
        }

        Ok(block_hash)
    }

    /// [`add_vote`] adds a vote to the current view's context.
    /// It returns a [`CollectedVotesResult`] indicating if the current replica
    /// has collected enough votes to propose a M-notarization or finalize the view.
    pub fn add_vote(&mut self, vote: Vote, peers: &PeerSet) -> Result<CollectedVotesResult> {
        if peers.contains(&vote.peer_id) {
            return Err(anyhow::anyhow!(
                "Vote for peer {} is not present in the peers set",
                vote.peer_id
            ));
        }

        if self.votes.iter().any(|v| v.peer_id == vote.peer_id) {
            return Err(anyhow::anyhow!(
                "Vote for peer {} already exists",
                vote.peer_id
            ));
        }

        if self.block_hash.is_none() {
            // NOTE: In this case, the replica has not yet received the view proposed block hash
            // from the leader, so we need to store the vote in the non-verified votes set.
            self.non_verified_votes.insert(vote);
            return Ok(CollectedVotesResult {
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

        let is_enough_to_m_notarize = self.votes.len() > 2 * F;
        let is_enough_to_finalize = self.votes.len() > N - F;

        self.votes.insert(vote);

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
            ));
        }

        Ok(CollectedVotesResult {
            is_enough_to_m_notarize,
            is_enough_to_finalize,
        })
    }

    /// [`add_nullify`] adds a nullify message to the current view's context.
    /// It returns a [`CollectedNullifyResult`] indicating if the current replica
    /// has collected enough nullify messages to propose a nullification.
    pub fn add_nullify(&mut self, nullify: Nullify) -> Result<CollectedNullifyResult> {
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

        let is_enough_for_nullification = self.nullify_messages.len() > 2 * F;

        self.nullify_messages.insert(nullify);

        if is_enough_for_nullification {
            let NullificationData {
                peer_ids,
                aggregated_signature,
            } = create_nullification_data::<M_SIZE>(&self.nullify_messages)?;
            self.nullification = Some(Nullification::new(
                self.view_number,
                self.leader_id,
                aggregated_signature,
                peer_ids,
            ));
        }

        Ok(CollectedNullifyResult {
            is_enough_for_nullification,
        })
    }

    /// [`is_leader`] checks if the current replica is the leader for the current view.
    #[inline]
    pub fn is_leader(&self) -> bool {
        self.leader_id == self.replica_id
    }
}

/// [`CollectedVotesResult`] is the result of collecting votes for the current view's block.
/// It is used to determine if the current replica should propose a M-notarization or finalize the view.
pub struct CollectedVotesResult {
    /// Whether the current replica has collected enough votes to propose a M-notarization
    pub is_enough_to_m_notarize: bool,
    /// Whether the current replica has collected enough votes to finalize the view
    pub is_enough_to_finalize: bool,
}

/// [`CollectedNullifyResult`] is the result of collecting nullify messages for the current view.
/// It is used to determine if the current replica should propose a nullification.
pub struct CollectedNullifyResult {
    /// Whether the current replica has collected enough nullify messages to propose a nullification
    pub is_enough_for_nullification: bool,
}

// ... existing code ...

#[cfg(test)]
mod tests {
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
        secret_keys: Vec<BlsSecretKey>,
    }

    // Helper function to create a test peer set with secret keys
    fn create_test_peer_setup(size: usize) -> TestPeerSetup {
        let mut rng = thread_rng();
        let mut public_keys = vec![];
        let mut secret_keys = vec![];

        for _ in 0..size {
            let sk = BlsSecretKey::generate(&mut rng);
            let pk = sk.public_key();
            secret_keys.push(sk);
            public_keys.push(pk);
        }

        TestPeerSetup {
            peer_set: PeerSet::new(public_keys),
            secret_keys,
        }
    }

    // Helper function to create a signed vote from a peer
    fn create_test_vote(
        peer_index: usize,
        view: u64,
        block_hash: [u8; blake3::OUT_LEN],
        setup: &TestPeerSetup,
    ) -> Vote {
        let peer_id = setup.peer_set.sorted_peer_ids[peer_index];
        let secret_key = &setup.secret_keys[peer_index];
        let signature = secret_key.sign(&block_hash);

        Vote::new(view, block_hash, signature, peer_id)
    }

    // Helper function to create a test view context
    fn create_test_view_context<'a>(
        view_number: u64,
        leader_id: PeerId,
        replica_id: PeerId,
        parent_block_hash: [u8; blake3::OUT_LEN],
    ) -> ViewContext<4, 1, 96> {
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
        assert_eq!(result.unwrap(), expected_hash);
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

        let vote = create_test_vote(1, 10, [9u8; blake3::OUT_LEN], &setup);
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
}
