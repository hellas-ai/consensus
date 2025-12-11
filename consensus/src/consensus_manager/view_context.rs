//! View Context - Single-View State Management for Minimmit Consensus
//!
//! This module implements the [`ViewContext`], which encapsulates all consensus-related state
//! for a single view in the Minimmit Byzantine Fault Tolerant (BFT) consensus protocol. Each
//! view represents one round of consensus with a designated leader who proposes a block, and
//! replicas vote on the proposal or send nullify messages if consensus cannot be reached.
//!
//! ## Overview
//!
//! In the Minimmit protocol, consensus progresses through numbered **views**. Each view has:
//! - A **leader** (determined by a leader selection strategy, typically round-robin)
//! - A **block proposal** from the leader
//! - **Votes** from replicas (including the leader's implicit vote)
//! - **M-notarization** (2F+1 votes) allowing view progression
//! - **L-notarization** (N-F votes) finalizing the block permanently
//! - **Nullification** (2F+1 nullify messages) for timeouts or Byzantine behavior
//!
//! The `ViewContext` manages all this state and provides methods to:
//! - Add and validate block proposals, votes, and nullifications
//! - Detect when thresholds are reached (M-notarization, L-notarization, nullification)
//! - Track Byzantine behavior (conflicting votes, multiple proposals)
//! - Manage non-verified messages that arrive before the block proposal
//! - Determine when the replica should vote, nullify, or progress views
//!
//! ## Architecture
//!
//!
//! ┌──────────────────────────────────────────────────────────────────┐
//! │                      ViewContext                                 │
//! │                   (State for View V)                             │
//! │                                                                  │
//! │  ┌────────────────────────────────────────────────────────┐    │
//! │  │  Block Proposal State                                  │    │
//! │  │  - block: Option<Block>                                │    │
//! │  │  - block_hash: Option<[u8; 32]>                        │    │
//! │  │  - pending_block: Option<Block> (awaiting parent)      │    │
//! │  └────────────────────────────────────────────────────────┘    │
//! │                                                                  │
//! │  ┌────────────────────────────────────────────────────────┐    │
//! │  │  Vote Tracking                                         │    │
//! │  │  - votes: HashSet<Vote> (verified, matching block)     │    │
//! │  │  - non_verified_votes: HashSet<Vote> (no block yet)    │    │
//! │  │  - num_invalid_votes: usize (conflicting votes)        │    │
//! │  └────────────────────────────────────────────────────────┘    │
//! │                                                                  │
//! │  ┌────────────────────────────────────────────────────────┐    │
//! │  │  Notarization State                                    │    │
//! │  │  - m_notarization: Option<MNotarization> (>2F votes)   │    │
//! │  │  - (L-notarization created by ViewProgressManager)     │    │
//! │  └────────────────────────────────────────────────────────┘    │
//! │                                                                  │
//! │  ┌────────────────────────────────────────────────────────┐    │
//! │  │  Nullification State                                   │    │
//! │  │  - nullify_messages: HashSet<Nullify>                  │    │
//! │  │  - nullification: Option<Nullification> (>2F nullifies)│    │
//! │  └────────────────────────────────────────────────────────┘    │
//! │                                                                  │
//! │  ┌────────────────────────────────────────────────────────┐    │
//! │  │  Replica Participation Status                          │    │
//! │  │  - has_voted: bool                                     │    │
//! │  │  - has_nullified: bool                                 │    │
//! │  │  - has_proposed: bool (if leader)                      │    │
//! │  │  - entered_at: Instant (for timeout detection)         │    │
//! │  └────────────────────────────────────────────────────────┘    │
//! │                                                                  │
//! │  ┌────────────────────────────────────────────────────────┐    │
//! │  │  Pending State (Validation)                            │    │
//! │  │  - state_diff: Option<Arc<StateDiff>>                  │    │
//! │  └────────────────────────────────────────────────────────┘    │
//! └──────────────────────────────────────────────────────────────────┘
//!
//! ## State Transitions
//!
//! A typical view progresses through these states:
//!
//!
//! 1. VIEW_START ↓
//! 2. BLOCK_PROPOSED (leader proposes, gets implicit vote) ↓
//! 3. VOTING (replicas cast votes) ↓
//! 4. M_NOTARIZATION (>2F votes collected) ↓
//! 5. L_NOTARIZATION (≥N-F votes collected, block finalized) ↓
//! 6. VIEW_END (progress to next view)
//!
//! Alternatively, if consensus fails:
//!
//!
//! 1. VIEW_START ↓
//! 2. TIMEOUT or BYZANTINE_DETECTED ↓
//! 3. NULLIFY_MESSAGES (replicas send nullifications) ↓
//! 4. NULLIFICATION (>2F nullify messages collected) ↓
//! 5. VIEW_END (progress to next view without finalizing block)
//!
//! ## Key Concepts
//!
//! ### Leader's Implicit Vote
//!
//! Per the Minimmit protocol, when a leader proposes a block, they implicitly vote for it.
//! This implicit vote is automatically added to the vote set when `add_new_view_block` is called,
//! using the leader's block signature as the vote signature.
//!
//! ### Non-Verified Messages
//!
//! Due to network asynchrony, votes or M-notarizations may arrive before the block proposal.
//! These are stored in `non_verified_votes` or as a non-verified `m_notarization` until the
//! block arrives. Once the block is received, messages are verified:
//! - Matching votes are moved to the verified `votes` set
//! - Non-matching votes are counted as `num_invalid_votes`
//!
//! ### Byzantine Behavior Detection
//!
//! The `ViewContext` tracks several types of Byzantine behavior:
//!
//! 1. **Invalid Votes**: Votes for a different block hash than the leader proposed
//! 2. **Conflicting M-notarizations**: Multiple M-notarizations with different block hashes
//! 3. **Nullify Messages**: Explicit statements that a replica detected problems
//!
//! When >2F conflicting messages are detected (invalid votes + nullify messages), the
//! `should_nullify` flag is set, indicating the replica should create a nullification.
//!
//! ### Nullification Types
//!
//! There are two distinct types of nullification:
//!
//! 1. **Timeout Nullification** (`create_nullify_for_timeout`):
//!    - Occurs when a replica times out waiting for a block proposal
//!    - Can ONLY be called BEFORE voting
//!    - Indicates network delay or inactive leader, not necessarily Byzantine behavior
//!
//! 2. **Byzantine Nullification** (`create_nullify_for_byzantine`):
//!    - Occurs when >2F conflicting messages are detected
//!    - Can be called BEFORE or AFTER voting
//!    - Indicates definite Byzantine behavior in the system
//!
//! ### State Diff (Pending State)
//!
//! The `state_diff` field stores pre-computed state changes from block validation:
//!
//! 1. **Setting**: When a block is validated by the validation service, the resulting [`StateDiff`]
//!    is stored in the `ViewContext` via [`ViewChain::store_state_diff`].
//!
//! 2. **Usage**: When the view achieves M-notarization, the [`ViewChain`] calls
//!    [`on_m_notarization`](super::view_chain::ViewChain::on_m_notarization) which adds the
//!    `state_diff` to pending state via [`PendingStateWriter::add_m_notarized_diff`].
//!
//! 3. **Purpose**: This enables transaction validation to see speculative state from M-notarized
//!    (but not yet L-notarized) blocks, improving transaction throughput by allowing validation
//!    against the latest known state.
//!
//! Note: The `state_diff` is independent of the `block` field—it can be set before or
//! after the block arrives, depending on network message ordering.
//!
//! ## Thresholds
//!
//! For N replicas with F Byzantine faults (N ≥ 3F+1):
//!
//! - **M-notarization**: Requires >2F votes (e.g., 3 votes when N=6, F=1)
//!   - Allows view progression but does NOT finalize the block
//! - **L-notarization**: Requires ≥N-F votes (e.g., 5 votes when N=6, F=1)
//!   - Finalizes the block permanently (cannot be reverted)
//! - **Nullification**: Requires >2F nullify messages
//!   - Allows view progression without finalizing any block
//! - **Byzantine Detection**: Triggered by >2F conflicting messages
//!   - Prompts honest replicas to nullify the view
//!
//! ## Validation Rules
//!
//! The `ViewContext` enforces strict validation:
//!
//! - **View Number**: All messages must match the context's view number
//! - **Leader ID**: All messages must reference the correct leader
//! - **Peer Membership**: All signers must be in the `PeerSet`
//! - **Signatures**: All BLS signatures must be valid
//! - **No Duplicates**: At most one vote/nullify per peer per view
//! - **Minimmit Invariants**:
//!   - Cannot vote after nullifying
//!   - Cannot timeout-nullify after voting (but can Byzantine-nullify)
//!   - Only one block per view
//!
//! ## Result Types
//!
//! Methods return specialized result types indicating what action the replica should take:
//!
//! - [`LeaderProposalResult`]: After adding a block proposal
//!   - `should_vote`: Replica should vote for the block
//!   - `is_enough_to_m_notarize`: M-notarization threshold reached
//!   - `is_enough_to_finalize`: L-notarization threshold reached
//!   - `should_nullify`: Byzantine behavior detected
//!
//! - [`CollectedVotesResult`]: After adding a vote
//!   - `should_await`: Wait for block proposal
//!   - `is_enough_to_m_notarize`: M-notarization threshold reached
//!   - `is_enough_to_finalize`: L-notarization threshold reached
//!   - `should_nullify`: Byzantine behavior detected
//!   - `should_vote`: Replica should vote (for non-verified scenario)
//!
//! - [`ShouldMNotarize`]: After adding an M-notarization
//!   - `should_notarize`: Should create/broadcast M-notarization
//!   - `should_vote`: Should vote for the M-notarization's block
//!   - `should_nullify`: Conflicting M-notarization detected
//!   - `should_forward`: Should forward M-notarization to other replicas (exactly once)
//!
//! - [`CollectedNullificationsResult`]: After adding a nullification
//!   - `should_broadcast_nullification`: Should broadcast to other replicas (exactly once)
//!
//! ## Usage Example
//!
//! ```rust,ignore
//! use consensus::consensus_manager::view_context::ViewContext;
//! use consensus::state::peer::PeerSet;
//!
//! # fn example() -> anyhow::Result<()> {
//! // Create context for view 5 with leader 0
//! let view_number = 5;
//! let leader_id = 0;
//! let replica_id = 1; // This replica's ID
//! let parent_hash = [0u8; 32];
//! let mut context = ViewContext::<6, 1, 3>::new(
//!     view_number,
//!     leader_id,
//!     replica_id,
//!     parent_hash,
//! );
//!
//! // Leader proposes a block
//! let result = context.add_new_view_block(block, &peers)?;
//! if result.should_vote {
//!     // Replica should vote for this block
//!     let vote = create_vote(block.get_hash());
//!     context.add_own_vote(vote_signature)?;
//! }
//!
//! // Receive votes from other replicas
//! let vote_result = context.add_vote(incoming_vote, &peers)?;
//! if vote_result.is_enough_to_m_notarize {
//!     // M-notarization threshold reached - can progress view
//!     let m_not = context.m_notarization.as_ref().unwrap();
//!     broadcast_m_notarization(m_not);
//! }
//! if vote_result.should_nullify {
//!     // Byzantine behavior detected - create nullification
//!     let nullify = context.create_nullify_for_byzantine(&secret_key)?;
//!     broadcast_nullify(nullify);
//! }
//!
//! // Check for timeout
//! if context.should_timeout_nullify(timeout_duration) {
//!     let nullify = context.create_nullify_for_timeout(&secret_key)?;
//!     broadcast_nullify(nullify);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Minimmit Paper Correspondence
//!
//! This implementation follows the Minimmit research paper:
//!
//! - **Algorithm 1, Lines 4-8**: Block proposal and leader implicit vote → `add_new_view_block`
//! - **Algorithm 1, Lines 9-10**: Voting logic → `add_vote`, `add_own_vote`
//! - **Algorithm 1, Lines 11-14**: M-notarization creation → automatic in `add_vote`
//! - **Algorithm 1, Lines 15-17**: L-notarization → detected via `is_enough_to_finalize`
//! - **Algorithm 1, Lines 18-20**: Timeout nullification → `create_nullify_for_timeout`
//! - **Algorithm 1, Lines 21-25**: Byzantine nullification → `create_nullify_for_byzantine`
//! - **Algorithm 1, Lines 26-28**: Nullification aggregation → `add_nullify`, automatic creation
//! - **Section 3 (Validity)**: Signature verification enforced in all `add_*` methods
//! - **Section 4 (Safety)**: Vote-once, nullify-once enforced via state flags
//!
//! ## Thread Safety
//!
//! `ViewContext` is **not** thread-safe and should only be accessed from the consensus state
//! machine thread. It is typically owned by a [`ViewChain`](super::view_chain::ViewChain) which
//! manages multiple view contexts.
//!
//! ## Performance Considerations
//!
//! - **Vote Storage**: Uses `HashSet` for O(1) duplicate detection
//! - **Signature Verification**: BLS signatures are verified on every message add
//! - **M-notarization Creation**: Automatically created when threshold is reached, uses BLS
//!   signature aggregation (constant size regardless of vote count)
//! - **Memory**: Each view context stores all votes, nullify messages, and the block
//!
//! ## Testing
//!
//! The module includes comprehensive unit tests covering:
//! - Normal voting flows (M-notarization, L-notarization)
//! - Byzantine scenarios (conflicting votes, multiple blocks)
//! - Timeout handling
//! - Non-verified message handling
//! - Nullification flows (both timeout and Byzantine)
//! - Edge cases (duplicate messages, wrong view numbers, invalid signatures)
//! - Integration scenarios (concurrent M-notarization and nullification, message ordering)

use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};

use crate::{
    consensus_manager::utils::{
        NotarizationData, NullificationData, create_notarization_data, create_nullification_data,
    },
    crypto::aggregated::{BlsSecretKey, BlsSignature, PeerId},
    state::{
        block::Block,
        notarizations::{MNotarization, Vote},
        nullify::{Nullification, Nullify},
        peer::PeerSet,
    },
    validation::StateDiff,
};

/// State tracking for a single view in the consensus protocol.
///
/// A [`ViewContext`] encapsulates all consensus-related state for a specific view, including
/// the block proposal, collected votes, nullifications, and the replica's own participation
/// status. Each view has a designated leader who proposes a block, and replicas vote on
/// proposals or send nullify messages if consensus cannot be reached.
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

    /// The number of received invalid votes for the current view's block. That is,
    /// votes for a different block hash than the block hash received by the leader.
    pub num_invalid_votes: usize,

    /// Received votes for the current view's block
    pub votes: HashSet<Vote>,

    /// Non-verified votes for the current view's block
    pub non_verified_votes: HashSet<Vote>,

    /// A m-notarization for the current view (if any)
    pub m_notarization: Option<MNotarization<N, F, M_SIZE>>,

    /// Received nullify messages for the current view
    pub nullify_messages: HashSet<Nullify>,

    /// Pre-computed state diff from block validation.
    ///
    /// This field stores the [`StateDiff`] produced when the block's transactions are validated
    /// and executed. It is set by [`ViewChain::store_state_diff`] when block validation completes.
    pub state_diff: Option<Arc<StateDiff>>,

    /// A nullification for the current view (if any)
    pub nullification: Option<Nullification<N, F, M_SIZE>>,

    /// Pending block proposal awaiting parent notarization
    pub pending_block: Option<Block>,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> ViewContext<N, F, M_SIZE> {
    /// Creates a new [`ViewContext`] instance
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
            num_invalid_votes: 0,
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
            state_diff: None,
            leader_id,
            pending_block: None,
        }
    }

    /// Adds a proposed block to the current view's context.
    ///
    /// Validates the block's view number, leader, and parent hash match the current view.
    /// Sets the block hash and moves any non-verified votes to the verified set.
    /// Returns whether enough votes exist for M-notarization or finalization.
    ///
    /// Returns an error if the block is invalid.
    pub fn add_new_view_block(
        &mut self,
        block: Block,
        peers: &PeerSet,
    ) -> Result<LeaderProposalResult> {
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
        let leader_public_key = peers.get_public_key(&self.leader_id)?;
        if !leader_public_key.verify(&block_hash, &block.leader_signature) {
            return Err(anyhow!(
                "Block signature is not valid for leader {}",
                self.leader_id
            ));
        }

        if let Some(ref m_not) = self.m_notarization
            && m_not.block_hash != block_hash
        {
            // Byzantine behavior detected: Leader proposed a different block
            // than what 2f+1 replicas M-notarized
            // This indicates either a Byzantine leader or fraudulent M-notarization

            // We should NOT accept this block and should nullify
            return Ok(LeaderProposalResult {
                block_hash,
                should_vote: false, // Don't vote for this conflicting block
                is_enough_to_m_notarize: false,
                is_enough_to_finalize: false,
                should_await: false,
                should_nullify: true,
            });
        }

        let block_signature = block.leader_signature.clone();
        self.block_hash = Some(block_hash);
        self.block = Some(block);

        let leader_vote = Vote::new(
            self.view_number,
            block_hash,
            block_signature,
            self.leader_id,
            self.leader_id,
        );
        self.votes.insert(leader_vote);

        if !self.non_verified_votes.is_empty() {
            let mut num_matching_votes = 0;
            self.non_verified_votes
                .iter()
                .filter(|v| v.block_hash == block_hash)
                .for_each(|v| {
                    self.votes.insert(v.clone());
                    num_matching_votes += 1;
                });
            self.num_invalid_votes += self.non_verified_votes.len() - num_matching_votes;
            self.non_verified_votes.clear();
        }

        if self.should_nullify_after_receiving_new_vote() {
            return Ok(LeaderProposalResult {
                block_hash,
                should_vote: false,
                is_enough_to_m_notarize: false,
                is_enough_to_finalize: false,
                should_await: false,
                should_nullify: true,
            });
        }

        let is_enough_to_m_notarize = self.votes.len() > 2 * F || self.m_notarization.is_some();
        let is_enough_to_finalize = self.votes.len() >= N - F;

        if is_enough_to_finalize && let Some(block) = self.block.as_mut() {
            block.is_finalized = true;
        }

        let should_vote = !self.has_voted && !self.has_nullified;

        Ok(LeaderProposalResult {
            block_hash,
            should_vote,
            is_enough_to_m_notarize,
            is_enough_to_finalize,
            should_await: false,
            should_nullify: false,
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
                "Vote signature is not valid for peer {}, view number {}",
                vote.peer_id,
                vote.view
            ));
        }

        if self.block_hash.is_none() {
            // NOTE: In this case, the replica has not yet received the view proposed block hash
            // from the leader, so we need to store the vote in the non-verified votes set.
            let should_vote = !self.has_voted
                && !self.has_nullified
                && self
                    .non_verified_votes
                    .iter()
                    .filter(|v| v.block_hash == vote.block_hash)
                    .count() + 1 // NOTE: We need to include the current vote in the count
                    > 2 * F;
            self.non_verified_votes.insert(vote);

            return Ok(CollectedVotesResult {
                should_await: true,
                is_enough_to_m_notarize: false,
                is_enough_to_finalize: false,
                should_nullify: self.should_nullify_after_receiving_new_vote(),
                should_vote,
            });
        }

        let block_hash = self.block_hash.unwrap();

        if vote.block_hash != block_hash {
            self.num_invalid_votes += 1;
            let should_nullify = self.should_nullify_after_receiving_new_vote();
            return Ok(CollectedVotesResult {
                should_await: false,
                is_enough_to_m_notarize: false,
                is_enough_to_finalize: false,
                should_nullify,
                should_vote: false,
            });
        }

        self.votes.insert(vote);

        let should_nullify = self.should_nullify_after_receiving_new_vote();
        let is_enough_to_m_notarize = (self.votes.len() > 2 * F) && self.m_notarization.is_none();
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
            should_nullify,
            should_vote: false,
        })
    }

    /// Adds the current replica's own vote to the current view's context.
    pub fn add_own_vote(
        &mut self,
        block_hash: [u8; blake3::OUT_LEN],
        signature: BlsSignature,
    ) -> Result<()> {
        if self.has_voted {
            return Err(anyhow::anyhow!("Replica has already voted"));
        }

        if let Some(current_hash) = self.block_hash {
            if current_hash != block_hash {
                return Err(anyhow::anyhow!(
                    "Replica trying to vote for block hash {:?} but context already has block hash {:?}",
                    block_hash,
                    current_hash
                ));
            }
        } else {
            self.block_hash = Some(block_hash);
        }

        self.has_voted = true;
        self.votes.insert(Vote::new(
            self.view_number,
            block_hash,
            signature,
            self.replica_id,
            self.leader_id,
        ));

        // Move non-verified votes that match the chosen block hash to the verified set
        // and count invalid votes
        if !self.non_verified_votes.is_empty() {
            let mut num_matching_votes = 0;

            self.non_verified_votes
                .iter()
                .filter(|v| v.block_hash == block_hash)
                .for_each(|v| {
                    self.votes.insert(v.clone());
                    num_matching_votes += 1;
                });

            self.num_invalid_votes += self.non_verified_votes.len() - num_matching_votes;
            self.non_verified_votes.clear();
        }

        Ok(())
    }

    /// Adds a leader vote for a block proposal to the current view's context.
    ///
    /// Validates the block's view number, leader, and parent hash match the current view.
    /// Stores the vote in the votes set.
    ///
    /// Returns an error if validation fails.
    pub fn add_leader_vote_for_block_proposal(
        &mut self,
        block: Block,
        signature: BlsSignature,
    ) -> Result<()> {
        if block.view() != self.view_number {
            return Err(anyhow::anyhow!(
                "Leader vote for block proposal for view {} is not the current view {}",
                block.view(),
                self.view_number
            ));
        }

        if block.leader != self.leader_id {
            return Err(anyhow::anyhow!(
                "Leader vote for block proposal for leader {} is not the current leader {}",
                block.leader,
                self.leader_id
            ));
        }

        if block.parent_block_hash() != self.parent_block_hash {
            return Err(anyhow::anyhow!(
                "Leader vote for block proposal for parent block hash {} is not the current parent block hash {}",
                hex::encode(block.parent_block_hash()),
                hex::encode(self.parent_block_hash)
            ));
        }

        self.votes.insert(Vote::new(
            self.view_number,
            block.get_hash(),
            signature,
            self.leader_id,
            self.leader_id,
        ));

        Ok(())
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

        self.nullify_messages.insert(nullify);
        let is_enough_for_nullification = self.nullify_messages.len() > 2 * F;

        if is_enough_for_nullification {
            let NullificationData {
                peer_ids,
                aggregated_signature,
            } = create_nullification_data::<M_SIZE>(&self.nullify_messages)?;
            if self.block.is_some() {
                self.block.as_mut().unwrap().is_finalized = true;
            }

            self.nullification = Some(Nullification::new(
                self.view_number,
                self.leader_id,
                aggregated_signature,
                peer_ids,
            ));

            // Set has_nullified flag when nullification is created
            self.has_nullified = true;
        }

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

        let should_vote = !self.has_voted && !self.has_nullified;

        if self.block_hash.is_none() {
            // If we receive an M-notarization but haven't seen the block yet,
            // we accept the block hash from the M-notarization as the valid one for this view.
            // This ensures that future chain validation (e.g., find_parent_view) can find this
            // view.
            self.block_hash = Some(m_notarization.block_hash);

            // Process any non-verified votes that match this hash
            if !self.non_verified_votes.is_empty() {
                let block_hash = m_notarization.block_hash;
                let mut num_matching_votes = 0;

                self.non_verified_votes
                    .iter()
                    .filter(|v| v.block_hash == block_hash)
                    .for_each(|v| {
                        self.votes.insert(v.clone());
                        num_matching_votes += 1;
                    });

                self.num_invalid_votes += self.non_verified_votes.len() - num_matching_votes;
                self.non_verified_votes.clear();
            }

            if self.should_nullify_after_receiving_new_vote() {
                return Ok(ShouldMNotarize {
                    should_notarize: false,
                    should_await: false,
                    should_vote: false,
                    should_nullify: true,
                    should_forward: false,
                });
            }

            let should_forward = if self.m_notarization.is_none() {
                self.m_notarization = Some(m_notarization.clone());
                true
            } else {
                false
            };
            return Ok(ShouldMNotarize {
                should_notarize: false,
                should_await: false,
                should_vote,
                should_nullify: false,
                should_forward,
            });
        }

        let block_hash = self.block_hash.unwrap();
        if m_notarization.block_hash != block_hash {
            // Byzantine behavior: M-notarization for a different block than what we have
            // This could mean either:
            // 1. Byzantine leader sent us a different block than what was M-notarized
            // 2. Byzantine replicas created an M-notarization for a different block
            // Either way, we should nullify this view
            return Ok(ShouldMNotarize {
                should_notarize: false,
                should_await: false,
                should_vote: false,    // Don't vote if there's conflict
                should_nullify: true,  // Signal that we should nullify this view
                should_forward: false, // Don't forward conflicting M-notarization
            });
        }

        if self.m_notarization.is_some() {
            let existing_m_not = self.m_notarization.as_ref().unwrap();

            // Check if this is a duplicate (same block hash) or a conflict (different block hash)
            if existing_m_not.block_hash == m_notarization.block_hash {
                // Duplicate M-notarization for the same block - safely ignore
                return Ok(ShouldMNotarize {
                    should_notarize: false,
                    should_await: false,
                    should_vote,
                    should_nullify: false,
                    should_forward: false,
                });
            } else {
                // CONFLICTING M-notarization:
                // Two different M-notarizations for the same view means Byzantine replicas
                // This is critical evidence that should trigger nullification
                return Ok(ShouldMNotarize {
                    should_notarize: false,
                    should_await: false,
                    should_vote: false,   // Don't vote if there's conflict
                    should_nullify: true, // Signal that we should nullify this view
                    should_forward: false, /* TODO: should we forward the conflicting
                                           * M-notarization to the network layer? */
                });
            }
        }

        self.m_notarization = Some(m_notarization);
        let should_notarize = self.nullification.is_none();

        Ok(ShouldMNotarize {
            should_notarize,
            should_await: false,
            should_vote,
            should_nullify: false,
            should_forward: true,
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

        if self.block.is_some() {
            self.block.as_mut().unwrap().is_finalized = true;
        }

        if self.nullification.is_some() {
            // NOTE: We already have a nullification for the current view, so we can safely ignore
            // this one.
            return Ok(CollectedNullificationsResult {
                should_broadcast_nullification: false,
            });
        }

        self.nullification = Some(nullification);
        self.has_nullified = true;

        Ok(CollectedNullificationsResult {
            should_broadcast_nullification: true,
        })
    }

    /// [`is_leader`] checks if the current replica is the leader for the current view.
    #[inline]
    pub fn is_leader(&self) -> bool {
        self.leader_id == self.replica_id
    }

    /// Checks if the view should be nullified due to timeout.
    /// This method should be called periodically by the [`ViewProgressManager`] to ensure the
    /// view is not left in an invalid state.
    pub fn should_timeout_nullify(&self, timeout_duration: Duration) -> bool {
        !self.has_nullified && !self.has_voted && self.entered_at.elapsed() >= timeout_duration
    }

    /// Creates a nullify message for timeout.
    /// This should be called when a replica times out waiting for a block proposal.
    /// The replica must NOT have voted yet.
    pub fn create_nullify_for_timeout(&mut self, secret_key: &BlsSecretKey) -> Result<Nullify> {
        if self.has_nullified {
            return Err(anyhow::anyhow!("Already nullified in this view"));
        }
        if self.has_voted {
            return Err(anyhow::anyhow!(
                "Cannot nullify for timeout after voting - use create_nullify_for_byzantine if conflicting evidence detected"
            ));
        }

        self.create_nullify_message(secret_key)
    }

    /// Creates a nullify message for Byzantine/conflicting evidence.
    /// This can be called BEFORE or AFTER voting when >2F conflicting messages are detected
    /// (invalid votes or other nullify messages).
    /// This is distinct from timeout nullification, which only occurs before voting.
    pub fn create_nullify_for_byzantine(&mut self, secret_key: &BlsSecretKey) -> Result<Nullify> {
        if self.has_nullified {
            return Err(anyhow::anyhow!("Already nullified in this view"));
        }

        self.create_nullify_message(secret_key)
    }

    /// Internal helper to create the actual nullify message
    fn create_nullify_message(&mut self, secret_key: &BlsSecretKey) -> Result<Nullify> {
        let message =
            blake3::hash(&[self.view_number.to_le_bytes(), self.leader_id.to_le_bytes()].concat());
        let signature = secret_key.sign(message.as_bytes());

        self.has_nullified = true;
        Ok(Nullify::new(
            self.view_number,
            self.leader_id,
            signature,
            self.replica_id,
        ))
    }

    /// [`has_view_progressed_without_m_notarization`] checks if the current view has progressed
    /// without a m-notarization. It returns an error if the current m-notarization is not
    /// present. This method is explicitly called by the view progress manager to ensure the
    /// `unfinalized_view_context` is not left in an invalid state.
    #[inline]
    pub fn has_view_progressed_without_m_notarization(&self) -> Result<()> {
        if self.m_notarization.is_none() && self.nullification.is_none() {
            return Err(anyhow::anyhow!(
                "View {} has progressed without M-notarization or nullification, which should never happen.",
                self.view_number
            ));
        }
        Ok(())
    }

    /// Public method to check if replica should nullify after having voted
    /// Called by ViewProgressManager to check for conflicting evidence
    pub fn should_nullify_after_receiving_new_vote(&self) -> bool {
        if self.has_nullified {
            return false;
        }

        // If we have > F conflicting votes (i.e. at least one honest replica voted for a different
        // block than the leader proposed), therefore finalization is impossible (can't get
        // N-F votes for any single block)
        if self.num_invalid_votes > F {
            return true;
        }

        // Combined evidence: nullify messages + invalid votes
        let conflicting_count = self.nullify_messages.len() + self.num_invalid_votes;
        conflicting_count > 2 * F
    }
}

/// [`LeaderProposalResult`] is the result of receiving a new view leader's block proposal.
/// In the unlikely case that a replica receives a block proposal for the current view, after
/// collecting enough votes, we allow it to step the state machine with either a m-notarization or a
/// finalization (if enough votes have been collected).
#[derive(Debug)]
pub struct LeaderProposalResult {
    /// The hash of the block that the replica should vote for
    pub block_hash: [u8; blake3::OUT_LEN],
    /// Whether the current replica should vote for the block hash.
    /// It might be the case that the replica has already voted for the block hash
    /// (if it received a M-notarization from peers before receiving the block proposal from the
    /// leader), or has nullified the view.
    pub should_vote: bool,
    /// Whether the current replica has collected enough votes to propose a M-notarization
    pub is_enough_to_m_notarize: bool,
    /// Whether the current replica has collected enough votes to finalize the view
    pub is_enough_to_finalize: bool,
    /// Whether the current replica should await for the current view's leader
    /// proposed block to be finalized before voting for it
    pub should_await: bool,
    /// Whether the current replica should nullify the current view, after receiving a block
    /// proposal for the current view, that is, the block hash is different from the
    /// M-notarization block hash
    pub should_nullify: bool,
}

/// [`CollectedVotesResult`] is the result of collecting votes for the current view's block.
/// It is used to determine if the current replica should propose a M-notarization or finalize the
/// view.
#[derive(Debug)]
pub struct CollectedVotesResult {
    /// Whether the current replica should await for the current view's leader to propose a block
    pub should_await: bool,
    /// Whether the current replica has collected enough votes to propose a M-notarization
    pub is_enough_to_m_notarize: bool,
    /// Whether the current replica has collected enough votes to finalize the view
    pub is_enough_to_finalize: bool,
    /// Whether the current replica should nullify the current view, after receiving enough invalid
    /// votes, that is, strictly more than 2 * F invalid votes.
    pub should_nullify: bool,
    /// Whether the current replica should vote for the block hash
    pub should_vote: bool,
}

/// [`ShouldMNotarize`] is the result of processing a newly received m-notarization for the current
/// view.
#[derive(Debug)]
pub struct ShouldMNotarize {
    /// Whether the current replica should notarize the current view's block
    pub should_notarize: bool,
    /// Whether the current replica should finalize the current view
    pub should_await: bool,
    /// If the current replica should vote for the M-notarization block hash,
    /// in case it hasn't voted yet and hasn't received a block proposal from the leader (yet)
    pub should_vote: bool,
    /// In case the replica receives a M-notarization for a different block hash than that
    /// of the leader proposed block, it should broadcast a nullify block
    pub should_nullify: bool,
    /// Whether the current replica should forward the m-notarization to the network layer
    pub should_forward: bool,
}

/// [`CollectedNullificationsResult`] is the result of collecting nullifications for the current
/// view. It is used to determine if the current replica should broadcast a nullification.
#[derive(Debug)]
pub struct CollectedNullificationsResult {
    /// Whether the current replica has collected enough nullifications to broadcast a
    /// nullification
    pub should_broadcast_nullification: bool,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::{
        crypto::{aggregated::BlsSecretKey, transaction_crypto::TxSecretKey},
        state::{address::Address, block::Block, transaction::Transaction},
    };
    use rand::thread_rng;

    // Helper function to generate a test transaction
    fn gen_tx() -> Transaction {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        Transaction::new_transfer(
            Address::from_public_key(&pk),
            Address::from_bytes([7u8; 32]),
            42,
            9,
            1_000,
            &sk,
        )
    }

    // Helper function to create a test block
    fn create_test_block(
        view: u64,
        leader: PeerId,
        parent_hash: [u8; blake3::OUT_LEN],
        leader_sk: BlsSecretKey,
        height: u64,
    ) -> Block {
        let transactions = vec![gen_tx()];

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

    // Helper function to create a signed nullify message from a peer
    fn create_test_nullify(
        peer_index: usize,
        view: u64,
        leader_id: PeerId,
        setup: &TestPeerSetup,
    ) -> Nullify {
        let peer_id = setup.peer_set.sorted_peer_ids[peer_index];
        let secret_key = setup.peer_id_to_secret_key.get(&peer_id).unwrap();

        // Create the message that needs to be signed (same as in Nullify::verify)
        let message = blake3::hash(&[view.to_le_bytes(), leader_id.to_le_bytes()].concat());
        let signature = secret_key.sign(message.as_bytes());

        Nullify::new(view, leader_id, signature, peer_id)
    }

    // Helper function to create a test M-notarization from votes
    fn create_test_m_notarization<const N: usize, const F: usize, const M_SIZE: usize>(
        votes: &HashSet<Vote>,
        view: u64,
        block_hash: [u8; blake3::OUT_LEN],
        leader_id: PeerId,
    ) -> MNotarization<N, F, M_SIZE> {
        let NotarizationData {
            peer_ids,
            aggregated_signature,
        } = create_notarization_data::<M_SIZE>(votes).unwrap();

        MNotarization::new(view, block_hash, aggregated_signature, peer_ids, leader_id)
    }

    // Helper function to create a test nullification from nullify messages
    fn create_test_nullification<const N: usize, const F: usize, const M_SIZE: usize>(
        nullify_messages: &HashSet<Nullify>,
        view: u64,
        leader_id: PeerId,
    ) -> Nullification<N, F, M_SIZE> {
        let NullificationData {
            peer_ids,
            aggregated_signature,
        } = create_nullification_data::<M_SIZE>(nullify_messages).unwrap();

        Nullification::new(view, leader_id, aggregated_signature, peer_ids)
    }

    // Helper function to create a test view context
    fn create_test_view_context(
        view_number: u64,
        leader_id: PeerId,
        replica_id: PeerId,
        parent_block_hash: [u8; blake3::OUT_LEN],
    ) -> ViewContext<4, 1, 3> {
        ViewContext::new(view_number, leader_id, replica_id, parent_block_hash)
    }

    /// Helper function to create a test view context with custom parameters
    fn create_test_view_context_with_params<const N: usize, const F: usize, const M_SIZE: usize>(
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

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(5, leader_id, parent_hash, leader_sk.clone(), 1);
        let expected_hash = block.get_hash();

        let result = context.add_new_view_block(block.clone(), &peers);

        assert!(result.is_ok());
        let proposal_result = result.unwrap();
        assert_eq!(proposal_result.block_hash, expected_hash);
        assert_eq!(context.block_hash, Some(expected_hash));
        assert_eq!(context.block, Some(block));
        // should_vote should be true if not voted and not nullified
        assert!(proposal_result.should_vote);
        assert!(!context.has_voted);
        assert!(!context.has_nullified);
    }

    #[test]
    fn test_add_new_view_block_should_not_vote_if_already_voted() {
        let setup = create_test_peer_setup(4);
        let peers = setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [2u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(5, leader_id, replica_id, parent_hash);

        // Mark as already voted
        context.has_voted = true;

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(5, leader_id, parent_hash, leader_sk.clone(), 1);
        let result = context.add_new_view_block(block, &peers);

        assert!(result.is_ok());
        let proposal_result = result.unwrap();
        // Should NOT vote if already voted (Minimmit: vote at most once per view)
        assert!(!proposal_result.should_vote);
    }

    #[test]
    fn test_add_new_view_block_should_not_vote_if_already_nullified() {
        let setup = create_test_peer_setup(4);
        let peers = setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [3u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(5, leader_id, replica_id, parent_hash);

        // Mark as already nullified
        context.has_nullified = true;

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(5, leader_id, parent_hash, leader_sk.clone(), 1);
        let result = context.add_new_view_block(block, &peers);

        assert!(result.is_ok());
        let proposal_result = result.unwrap();
        // Should NOT vote if already nullified (Minimit: cannot vote after nullify)
        assert!(!proposal_result.should_vote);
    }

    #[test]
    fn test_add_new_view_block_moves_non_verified_votes() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [4u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Create the block first to get its actual hash
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 2);
        let block_hash = block.get_hash();

        // Add a non-verified vote with the actual block hash
        let vote = create_test_vote(1, 10, block_hash, leader_id, &setup);
        context.non_verified_votes.insert(vote.clone());

        assert_eq!(context.non_verified_votes.len(), 1);
        assert_eq!(context.votes.len(), 0);

        // Now add the block
        let result = context.add_new_view_block(block, peers);

        assert!(result.is_ok());
        // Non-verified votes should be moved to verified votes
        assert_eq!(context.votes.len(), 2);
        assert_eq!(context.non_verified_votes.len(), 0);
        assert!(context.votes.contains(&vote));
    }

    #[test]
    fn test_add_new_view_block_with_non_verified_vote_for_different_hash() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [4u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Add a non-verified vote for a DIFFERENT block hash (e.g., Byzantine behavior)
        let wrong_block_hash = [99u8; blake3::OUT_LEN];
        let vote = create_test_vote(1, 10, wrong_block_hash, leader_id, &setup);
        context.non_verified_votes.insert(vote.clone());

        assert_eq!(context.non_verified_votes.len(), 1);
        assert_eq!(context.votes.len(), 0);
        assert_eq!(context.num_invalid_votes, 0);

        // Add the actual leader's block with a different hash
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 2);
        let actual_block_hash = block.get_hash();
        assert_ne!(actual_block_hash, wrong_block_hash); // Verify they're different

        let result = context.add_new_view_block(block, peers);
        assert!(result.is_ok());

        // Non-verified votes should be cleared
        assert_eq!(context.non_verified_votes.len(), 0);

        // Vote with wrong hash should NOT be in verified votes
        assert_eq!(context.votes.len(), 1);

        // Vote with wrong hash should be counted as invalid
        assert_eq!(context.num_invalid_votes, 1);
    }

    #[test]
    fn test_add_new_view_block_detects_m_notarization() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [5u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();

        // Add 3 non-verified votes (2f+1 = 3 for M-notarization)
        for i in 0..3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            context.non_verified_votes.insert(vote);
        }

        let result = context.add_new_view_block(block, peers);
        assert!(result.is_ok());
        let proposal_result = result.unwrap();

        // Should detect M-notarization with 3 votes (>2*F where F=1)
        assert!(proposal_result.is_enough_to_m_notarize);
        assert_eq!(context.votes.len(), 3);
    }

    #[test]
    fn test_add_new_view_block_wrong_view() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [6u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(15, leader_id, replica_id, parent_hash);

        // Create block with wrong view number
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(20, leader_id, parent_hash, leader_sk.clone(), 3);
        let result = context.add_new_view_block(block, peers);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Proposed block for view 20 is not the current view 15")
        );
    }

    #[test]
    fn test_add_new_view_block_duplicate_block() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [7u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(8, leader_id, replica_id, parent_hash);

        // Add first block
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block1 = create_test_block(8, leader_id, parent_hash, leader_sk.clone(), 4);
        assert!(context.add_new_view_block(block1, peers).is_ok());

        // Try to add second block (should fail - only one block per view)
        let block2 = create_test_block(8, leader_id, parent_hash, leader_sk.clone(), 5);
        let result = context.add_new_view_block(block2, peers);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Block for view 8 already exists")
        );
    }

    #[test]
    fn test_add_new_view_block_wrong_leader() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let correct_leader = peers.sorted_peer_ids[0];
        let wrong_leader = peers.sorted_peer_ids[1];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [8u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(12, correct_leader, replica_id, parent_hash);

        // Create block with wrong leader
        let leader_sk = setup.peer_id_to_secret_key.get(&wrong_leader).unwrap();
        let block = create_test_block(12, wrong_leader, parent_hash, leader_sk.clone(), 6);
        let result = context.add_new_view_block(block, peers);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains(&format!(
            "Proposed block for leader {} is not the current leader {}",
            wrong_leader, correct_leader
        )));
    }

    #[test]
    fn test_add_new_view_block_wrong_parent_hash() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let correct_parent = [9u8; blake3::OUT_LEN];
        let wrong_parent = [10u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(18, leader_id, replica_id, correct_parent);

        // Create block with wrong parent hash
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(18, leader_id, wrong_parent, leader_sk.clone(), 7);
        let result = context.add_new_view_block(block, peers);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("is not the current parent block hash")
        );
    }

    #[test]
    fn test_add_new_view_block_detects_l_notarization() {
        let setup = create_test_peer_setup(6); // N=6, F=1, so N-F=5 for L-notarization
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [52u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();

        // Add 5 non-verified votes (N-F = 5 for L-notarization where N=6, F=1)
        for i in 0..5 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            context.non_verified_votes.insert(vote);
        }

        let result = context.add_new_view_block(block.clone(), peers);
        assert!(result.is_ok());
        let proposal_result = result.unwrap();

        // Should detect both M-notarization and L-notarization
        assert!(proposal_result.is_enough_to_m_notarize); // >2*F = >2
        assert!(proposal_result.is_enough_to_finalize); // >=N-F = >=5
        assert_eq!(context.votes.len(), 5);

        assert_eq!(context.num_invalid_votes, 0);
        assert_eq!(context.non_verified_votes.len(), 0);
        assert_eq!(context.votes.len(), 5);
        assert!(context.votes.iter().all(|v| v.block_hash == block_hash));
        assert!(context.nullify_messages.is_empty());

        // NOTE: We don't form a M-notarization in this case, as this should be handled by the
        // ViewProgressManager.
        assert!(context.m_notarization.is_none());
        assert!(context.nullification.is_none());
        assert!(context.pending_block.is_none());

        // Block should be marked as finalized
        assert!(context.block.as_ref().unwrap().is_finalized);
    }

    #[test]
    fn test_add_new_view_block_l_notarization_exactly_at_threshold() {
        let setup = create_test_peer_setup(6); // N=6, F=1, N-F=5
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [53u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();

        // Add exactly N-F votes
        for i in 0..5 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            context.non_verified_votes.insert(vote);
        }

        let result = context.add_new_view_block(block, peers);
        assert!(result.is_ok());
        let proposal_result = result.unwrap();

        // At exactly N-F votes, should finalize
        assert!(proposal_result.is_enough_to_finalize);
    }

    #[test]
    fn test_add_new_view_block_below_l_notarization_threshold() {
        let setup = create_test_peer_setup(6); // N=6, F=1, N-F=5
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [54u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();

        // Add only 4 votes (below N-F = 5)
        for i in 0..4 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            context.non_verified_votes.insert(vote);
        }

        let result = context.add_new_view_block(block, peers);
        assert!(result.is_ok());
        let proposal_result = result.unwrap();

        // Should detect M-notarization but NOT L-notarization
        assert!(proposal_result.is_enough_to_m_notarize); // >2*F = >2
        assert!(!proposal_result.is_enough_to_finalize); // <N-F
    }

    #[test]
    fn test_add_new_view_block_with_mixed_non_verified_votes() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [55u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        let wrong_hash = [98u8; blake3::OUT_LEN];

        // Add 2 votes with correct hash
        for i in 0..2 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            context.non_verified_votes.insert(vote);
        }

        // Add 3 votes with wrong hash
        for i in 2..5 {
            let vote = create_test_vote(i, 10, wrong_hash, leader_id, &setup);
            context.non_verified_votes.insert(vote);
        }

        assert_eq!(context.non_verified_votes.len(), 5);

        let result = context.add_new_view_block(block, peers);
        assert!(result.is_ok());

        // Only matching votes should be in verified set
        assert_eq!(context.votes.len(), 2);
        // Non-matching votes should be counted as invalid
        assert_eq!(context.num_invalid_votes, 3);
        // All non-verified should be cleared
        assert_eq!(context.non_verified_votes.len(), 0);
    }

    #[test]
    fn test_add_new_view_block_should_await_always_false() {
        let setup = create_test_peer_setup(4);
        let peers = setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [56u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let result = context.add_new_view_block(block, &peers);

        assert!(result.is_ok());
        let proposal_result = result.unwrap();

        // should_await should always be false in add_new_view_block
        // (it's only true when block proposal is awaiting parent notarization,
        // which is handled in ViewChain.add_block_proposal, not here)
        assert!(!proposal_result.should_await);
    }

    #[test]
    fn test_add_new_view_block_with_empty_non_verified_votes() {
        let setup = create_test_peer_setup(4);
        let peers = setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [57u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Ensure non_verified_votes is empty
        assert!(context.non_verified_votes.is_empty());

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let result = context.add_new_view_block(block, &peers);

        assert!(result.is_ok());

        // Should work fine with no non-verified votes
        assert_eq!(context.votes.len(), 1);
        assert_eq!(context.num_invalid_votes, 0);
    }

    #[test]
    fn test_add_new_view_block_with_both_m_notarization_and_votes() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [58u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();

        // Add non-verified M-notarization
        let mut m_votes = HashSet::new();
        for i in 0..3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            m_votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&m_votes, 10, block_hash, leader_id);
        context.m_notarization = Some(m_notarization);

        // Also add additional non-verified votes (different from M-notarization votes)
        for i in 3..5 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            context.non_verified_votes.insert(vote);
        }

        let result = context.add_new_view_block(block, peers);
        assert!(result.is_ok());
        let proposal_result = result.unwrap();

        // M-notarization should be verified
        assert!(context.m_notarization.is_some());
        assert!(proposal_result.is_enough_to_m_notarize);

        // Additional votes should be added
        assert_eq!(context.votes.len(), 3);

        // With M-notarization already present, it should indicate M-notarization
        assert!(proposal_result.is_enough_to_m_notarize);
    }

    #[test]
    fn test_add_vote_success() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [11u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Set a block hash first
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Create and add a valid vote
        let vote = create_test_vote(2, 10, block_hash, leader_id, &setup);
        let result = context.add_vote(vote.clone(), peers);

        assert!(result.is_ok());
        let vote_result = result.unwrap();
        assert!(!vote_result.should_await);
        assert!(!vote_result.is_enough_to_m_notarize); // Only 1 vote, need >2*F=2
        assert!(!vote_result.is_enough_to_finalize); // Only 1 vote, need >=N-F=3
        assert!(context.votes.contains(&vote));
        assert_eq!(context.votes.len(), 1);
    }

    #[test]
    fn test_add_vote_reaches_m_notarization_threshold() {
        let setup = create_test_peer_setup(6); // N=6, F=1, so 2*F+1=3 for M-notarization
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [12u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Add votes until M-notarization threshold (>2*F = >2)
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            let result = context.add_vote(vote, peers);
            assert!(result.is_ok());

            let vote_result = result.unwrap();
            if i <= 2 {
                assert!(!vote_result.is_enough_to_m_notarize);
                assert!(context.m_notarization.is_none());
            } else {
                // At i=3, we have >2*F votes
                assert!(vote_result.is_enough_to_m_notarize);
                assert!(context.m_notarization.is_some());
            }
        }
    }

    #[test]
    fn test_add_vote_reaches_l_notarization_threshold() {
        let setup = create_test_peer_setup(6); // N=6, F=1, so N-F=5 for L-notarization
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [13u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Add votes until L-notarization threshold (>=N-F = >=5)
        for i in 1..=5 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            let result = context.add_vote(vote, peers);
            assert!(result.is_ok());

            let vote_result = result.unwrap();
            if i < 3 {
                // Before M-notarization threshold
                assert!(!vote_result.is_enough_to_m_notarize);
                assert!(!vote_result.is_enough_to_finalize);
                assert!(context.m_notarization.is_none());
            } else if i == 3 {
                // Exactly at M-notarization threshold - should create it
                assert!(vote_result.is_enough_to_m_notarize);
                assert!(!vote_result.is_enough_to_finalize);
                assert!(context.m_notarization.is_some()); // M-notarization created
            } else if i == 4 {
                // After M-notarization exists but before L-notarization
                assert!(!vote_result.is_enough_to_m_notarize); // Already created, so false
                assert!(!vote_result.is_enough_to_finalize);
                assert!(context.m_notarization.is_some()); // Still exists
            } else {
                // At i=5, we have >=N-F votes for L-notarization
                assert!(!vote_result.is_enough_to_m_notarize); // M-notarization already exists
                assert!(vote_result.is_enough_to_finalize);
                assert!(context.m_notarization.is_some());
            }
        }

        assert_eq!(context.votes.len(), 5);
        assert!(context.m_notarization.is_some());
        // Note: L-notarization is created separately by the ViewProgressManager
    }

    #[test]
    fn test_add_vote_for_wrong_block_hash_counts_as_invalid() {
        // Per Minimit paper Algorithm 1, line 21-24:
        // Votes for different blocks count towards nullification threshold
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [14u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Set a block hash first
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Create vote with DIFFERENT block hash
        let wrong_block_hash = [15u8; blake3::OUT_LEN];
        let vote = create_test_vote(1, 10, wrong_block_hash, leader_id, &setup);
        let result = context.add_vote(vote, peers);

        // Should return Ok (not error) - invalid votes are counted, not rejected
        assert!(result.is_ok());
        let vote_result = result.unwrap();
        assert!(!vote_result.should_await);
        assert!(!vote_result.is_enough_to_m_notarize);
        assert!(!vote_result.is_enough_to_finalize);
        assert!(!vote_result.should_nullify);

        // Should increment invalid votes counter
        assert_eq!(context.num_invalid_votes, 1);
        // Should NOT be added to verified votes
        assert_eq!(context.votes.len(), 0);
    }

    #[test]
    fn test_add_vote_without_block_hash_goes_to_non_verified() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [16u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Don't set block hash - should store in non-verified
        let vote = create_test_vote(2, 10, [17u8; blake3::OUT_LEN], leader_id, &setup);
        let result = context.add_vote(vote.clone(), peers);

        assert!(result.is_ok());
        let vote_result = result.unwrap();
        assert!(vote_result.should_await); // Should await block proposal
        assert!(!vote_result.is_enough_to_m_notarize);
        assert!(!vote_result.is_enough_to_finalize);
        assert!(context.non_verified_votes.contains(&vote));
        assert_eq!(context.votes.len(), 0);
    }

    #[test]
    fn test_add_vote_triggers_nullification_on_conflicting_evidence() {
        // Per Minimmit Algorithm 1, line 21-25:
        // If voted and receive >2f+1 conflicting messages, should nullify
        let setup = create_test_peer_setup(6); // N=6, F=1, need >2 conflicting
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [18u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Mark as voted
        context.has_voted = true;

        // Add 3 votes for DIFFERENT block (>2f+1=3 conflicting)
        let wrong_hash = [19u8; blake3::OUT_LEN];
        for i in 0..3 {
            let vote = create_test_vote(i, 10, wrong_hash, leader_id, &setup);
            let result = context.add_vote(vote, peers);
            assert!(result.is_ok());

            let vote_result = result.unwrap();
            if i < 1 {
                assert!(!vote_result.should_nullify);
            } else {
                // At i=1 (second vote), we have > F conflicting
                assert!(vote_result.should_nullify);
            }
        }

        assert_eq!(context.num_invalid_votes, 3);
    }

    #[test]
    fn test_add_vote_peer_not_in_set() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [20u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Create vote with invalid peer ID
        let invalid_peer_id = 999;
        let block_hash = [21u8; blake3::OUT_LEN];
        let secret_key = BlsSecretKey::generate(&mut thread_rng());
        let signature = secret_key.sign(&block_hash);
        let invalid_vote = Vote::new(10, block_hash, signature, invalid_peer_id, leader_id);

        let result = context.add_vote(invalid_vote, peers);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("is not present in the peers set")
        );
    }

    #[test]
    fn test_add_vote_wrong_view() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [22u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Create vote with wrong view number
        let vote = create_test_vote(1, 15, [23u8; blake3::OUT_LEN], leader_id, &setup);
        let result = context.add_vote(vote, peers);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Vote for view 15 is not the current view 10")
        );
    }

    #[test]
    fn test_add_vote_duplicate() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [24u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Add first vote
        let vote = create_test_vote(1, 10, block_hash, leader_id, &setup);
        assert!(context.add_vote(vote.clone(), peers).is_ok());

        // Try to add same vote again (should fail - at most one vote per processor)
        let result = context.add_vote(vote, peers);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_add_vote_invalid_signature() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [25u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Create vote with wrong secret key (invalid signature)
        let peer_id = peers.sorted_peer_ids[1];
        let wrong_secret_key = BlsSecretKey::generate(&mut thread_rng());
        let invalid_signature = wrong_secret_key.sign(&block_hash);
        let invalid_vote = Vote::new(10, block_hash, invalid_signature, peer_id, leader_id);

        let result = context.add_vote(invalid_vote, peers);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("signature is not valid")
        );
    }

    #[test]
    fn test_add_vote_duplicate_in_non_verified_set() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [26u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Add vote to non-verified set first (no block hash set)
        let vote = create_test_vote(1, 10, [27u8; blake3::OUT_LEN], leader_id, &setup);
        let result1 = context.add_vote(vote.clone(), peers);
        assert!(result1.is_ok());
        assert!(context.non_verified_votes.contains(&vote));

        // Try to add the same vote again (should fail)
        let result2 = context.add_vote(vote, peers);
        assert!(result2.is_err());
        assert!(
            result2
                .unwrap_err()
                .to_string()
                .contains("already exists in the non-verified votes set")
        );
    }

    #[test]
    fn test_add_vote_creates_valid_m_notarization() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [28u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Add exactly >2*F votes (3 votes where F=1)
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            context.add_vote(vote, peers).unwrap();
        }

        // Verify M-notarization was created correctly
        assert!(context.m_notarization.is_some());
        let m_not = context.m_notarization.as_ref().unwrap();
        assert_eq!(m_not.view, 10);
        assert_eq!(m_not.block_hash, block_hash);
        assert_eq!(m_not.leader_id, leader_id);
        assert_eq!(m_not.peer_ids.len(), 3);

        // Verify M-notarization is valid
        assert!(m_not.verify(peers));
    }

    #[test]
    fn test_add_vote_does_not_recreate_m_notarization() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [29u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Add 3 votes to create M-notarization
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            context.add_vote(vote, peers).unwrap();
        }

        let original_m_not = context.m_notarization.clone();
        assert!(original_m_not.is_some());

        // Add another vote (4th vote)
        let vote4 = create_test_vote(4, 10, block_hash, leader_id, &setup);
        let result = context.add_vote(vote4, peers);
        assert!(result.is_ok());

        // M-notarization should remain the same (not recreated)
        assert_eq!(context.m_notarization, original_m_not);
        assert_eq!(context.votes.len(), 4);
    }

    #[test]
    fn test_add_vote_should_not_nullify_if_not_voted() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [30u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // NOT voted yet
        assert!(!context.has_voted);

        // Add many invalid votes (>= 2*F + 1)
        let wrong_hash = [31u8; blake3::OUT_LEN];
        for i in 0..3 {
            let vote = create_test_vote(i, 10, wrong_hash, leader_id, &setup);
            let result = context.add_vote(vote, peers);
            assert!(result.is_ok());

            if i < 1 {
                assert!(!result.unwrap().should_nullify); // Need >=2
            } else {
                assert!(result.unwrap().should_nullify); // 2 votes for different blocks, should nullify
            }
        }
    }

    #[test]
    fn test_add_vote_should_not_nullify_if_already_nullified() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [32u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Mark as voted and nullified
        context.has_voted = true;
        context.has_nullified = true;

        // Add invalid votes (>2f+1)
        let wrong_hash = [33u8; blake3::OUT_LEN];
        for i in 0..5 {
            let vote = create_test_vote(i, 10, wrong_hash, leader_id, &setup);
            let result = context.add_vote(vote, peers);
            assert!(result.is_ok());

            // Should not trigger nullification if already nullified
            assert!(!result.unwrap().should_nullify);
        }
    }

    #[test]
    fn test_add_vote_nullification_with_mixed_evidence() {
        let setup = create_test_peer_setup(6); // F=1, need >2 conflicting
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [34u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Mark as voted
        context.has_voted = true;

        // Add 2 nullify messages (not enough alone)
        for i in 0..2 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            context.nullify_messages.insert(nullify);
        }

        // Now add 1 invalid vote (2 nullify + 1 invalid = 3 total = >2f+1)
        let wrong_hash = [35u8; blake3::OUT_LEN];
        let vote = create_test_vote(2, 10, wrong_hash, leader_id, &setup);
        let result = context.add_vote(vote, peers);

        assert!(result.is_ok());
        let vote_result = result.unwrap();

        // Should trigger nullification with combined evidence
        assert!(vote_result.should_nullify);
        assert_eq!(context.num_invalid_votes, 1);
        assert_eq!(context.nullify_messages.len(), 2);
    }

    #[test]
    fn test_add_vote_exactly_at_m_notarization_boundary() {
        let setup = create_test_peer_setup(6); // F=1, so >2*F = >2 (need 3)
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [36u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Add exactly 2*F votes (not enough)
        for i in 1..=2 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            let result = context.add_vote(vote, peers);
            assert!(result.is_ok());
            assert!(!result.unwrap().is_enough_to_m_notarize); // Need >2, not >=2
        }

        // Add one more vote (now >2*F)
        let vote3 = create_test_vote(3, 10, block_hash, leader_id, &setup);
        let result = context.add_vote(vote3, peers);
        assert!(result.is_ok());
        assert!(result.unwrap().is_enough_to_m_notarize); // Now >2*F
    }

    #[test]
    fn test_add_vote_exactly_at_l_notarization_boundary() {
        let setup = create_test_peer_setup(6); // N=6, F=1, so N-F = 5
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [37u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Add exactly N-F-1 votes (not enough)
        for i in 1..=4 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            let result = context.add_vote(vote, peers);
            assert!(result.is_ok());
            assert!(!result.unwrap().is_enough_to_finalize); // Need >=5, not >=4
        }

        // Add one more vote (now >=N-F)
        let vote5 = create_test_vote(5, 10, block_hash, leader_id, &setup);
        let result = context.add_vote(vote5, peers);
        assert!(result.is_ok());
        assert!(result.unwrap().is_enough_to_finalize); // Now >=N-F
    }

    #[test]
    fn test_add_vote_exactly_at_nullification_boundary() {
        let setup = create_test_peer_setup(6); // F=1, need >2 conflicting
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [38u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);
        context.has_voted = true;

        // Add exactly 2*F invalid votes (not enough)
        let wrong_hash = [39u8; blake3::OUT_LEN];
        for i in 0..2 {
            let vote = create_test_vote(i, 10, wrong_hash, leader_id, &setup);
            let result = context.add_vote(vote, peers);
            assert!(result.is_ok());
            if i < 1 {
                assert!(!result.unwrap().should_nullify); // Need >=2
            } else {
                assert!(result.unwrap().should_nullify); // 2 votes for different blocks, should nullify
            }
        }

        // Add one more invalid vote (now >2*F)
        let vote3 = create_test_vote(2, 10, wrong_hash, leader_id, &setup);
        let result = context.add_vote(vote3, peers);
        assert!(result.is_ok());
        assert!(result.unwrap().should_nullify); // Now >2*F conflicting
    }

    #[test]
    fn test_add_vote_m_notarization_with_nullification_check() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [40u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);
        context.has_voted = true;

        // Add 2 valid votes
        for i in 1..=2 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            context.add_vote(vote, peers).unwrap();
        }

        // Add 3 nullify messages (for nullification check)
        for i in 3..=5 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            context.nullify_messages.insert(nullify);
        }

        // Add 3rd valid vote (triggers M-notarization)
        let vote3 = create_test_vote(0, 10, block_hash, leader_id, &setup);
        let result = context.add_vote(vote3, peers);

        assert!(result.is_ok());
        let vote_result = result.unwrap();

        // Should trigger M-notarization
        assert!(vote_result.is_enough_to_m_notarize);
        assert!(context.m_notarization.is_some());

        // Should also indicate nullification due to nullify messages
        assert!(vote_result.should_nullify);
    }

    #[test]
    fn test_add_m_notarization_success() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [26u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Create M-notarization
        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 10, block_hash, leader_id);

        let result = context.add_m_notarization(m_notarization, peers);
        assert!(result.is_ok());

        let notarize_result = result.unwrap();
        assert!(notarize_result.should_notarize);
        assert!(!notarize_result.should_await);
        assert!(context.m_notarization.is_some());
    }

    #[test]
    fn test_add_m_notarization_should_vote_if_not_voted_yet() {
        // Per Minimmit Algorithm 1, line 18:
        // If receive M-notarization and haven't voted/nullified, should vote
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [27u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Replica hasn't voted or nullified
        assert!(!context.has_voted);
        assert!(!context.has_nullified);

        // Create M-notarization
        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 10, block_hash, leader_id);

        let result = context.add_m_notarization(m_notarization, peers);
        assert!(result.is_ok());

        let notarize_result = result.unwrap();
        // Should indicate that voting is required
        assert!(notarize_result.should_vote);
    }

    #[test]
    fn test_add_m_notarization_should_not_vote_if_already_voted() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [28u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Mark as already voted
        context.has_voted = true;

        // Create M-notarization
        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 10, block_hash, leader_id);

        let result = context.add_m_notarization(m_notarization, peers);
        assert!(result.is_ok());

        let notarize_result = result.unwrap();
        // Should NOT vote if already voted
        assert!(!notarize_result.should_vote);
    }

    #[test]
    fn test_add_m_notarization_should_not_vote_if_already_nullified() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [29u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Mark as already nullified
        context.has_nullified = true;

        // Create M-notarization
        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 10, block_hash, leader_id);

        let result = context.add_m_notarization(m_notarization, peers);
        assert!(result.is_ok());

        let notarize_result = result.unwrap();
        // Should NOT vote if already nullified (per Minimmit: no voting after nullify)
        assert!(!notarize_result.should_vote);
    }

    #[test]
    fn test_add_m_notarization_without_block_stores_as_non_verified() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [30u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Don't set block hash
        let block_hash = [31u8; blake3::OUT_LEN];
        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 10, block_hash, leader_id);

        let result = context.add_m_notarization(m_notarization, peers);
        assert!(result.is_ok());

        let notarize_result = result.unwrap();

        // Can't notarize yet - need to wait for block to verify the hash matches
        assert!(!notarize_result.should_notarize);
        assert!(!notarize_result.should_await);

        // M-notarization stored for later verification when block arrives
        assert!(context.m_notarization.is_some());
    }

    #[test]
    fn test_add_m_notarization_duplicate_ignores_second() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [34u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Create M-notarization
        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 10, block_hash, leader_id);

        // Add first time
        assert!(
            context
                .add_m_notarization(m_notarization.clone(), peers)
                .is_ok()
        );

        // Add second time (should be ignored, not error)
        let result = context.add_m_notarization(m_notarization, peers);
        assert!(result.is_ok());
        let notarize_result = result.unwrap();
        assert!(!notarize_result.should_notarize); // Should not notarize again
    }

    #[test]
    fn test_add_m_notarization_wrong_view() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [60u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Create M-notarization with wrong view
        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 15, block_hash, leader_id, &setup); // view 15
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 15, block_hash, leader_id);

        let result = context.add_m_notarization(m_notarization, peers);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("M-notarization for view 15 is not the current view 10")
        );
    }

    #[test]
    fn test_add_m_notarization_wrong_leader() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let correct_leader = peers.sorted_peer_ids[0];
        let wrong_leader = peers.sorted_peer_ids[1];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [61u8; blake3::OUT_LEN];
        let mut context = create_test_view_context_with_params::<6, 1, 3>(
            10,
            correct_leader,
            replica_id,
            parent_hash,
        );

        let leader_sk = setup.peer_id_to_secret_key.get(&correct_leader).unwrap();
        let block = create_test_block(10, correct_leader, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Create M-notarization with wrong leader
        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, wrong_leader, &setup);
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 10, block_hash, wrong_leader);

        let result = context.add_m_notarization(m_notarization, peers);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains(&format!(
            "M-notarization for leader {} is not the current leader {}",
            wrong_leader, correct_leader
        )));
    }

    #[test]
    fn test_add_m_notarization_invalid_signature() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [62u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Create M-notarization with invalid signature
        let peer_ids: [PeerId; 3] = [
            peers.sorted_peer_ids[0],
            peers.sorted_peer_ids[1],
            peers.sorted_peer_ids[2],
        ];
        let wrong_signature = BlsSecretKey::generate(&mut thread_rng()).sign(&[99u8; 32]);
        let invalid_m_notarization =
            MNotarization::new(10, block_hash, wrong_signature, peer_ids, leader_id);

        let result = context.add_m_notarization(invalid_m_notarization, peers);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("signature is not valid")
        );
    }

    #[test]
    fn test_add_m_notarization_should_not_replace_non_verified() {
        // Per implementation: only stores if m_notarization.is_none()
        // But what if non_verified_m_notarization already exists?
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [63u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Don't set block hash - add first non-verified M-notarization
        let block_hash1 = [64u8; blake3::OUT_LEN];
        let mut votes1 = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash1, leader_id, &setup);
            votes1.insert(vote);
        }
        let m_not1 = create_test_m_notarization::<6, 1, 3>(&votes1, 10, block_hash1, leader_id);

        let result1 = context.add_m_notarization(m_not1, peers);
        assert!(result1.is_ok());
        assert!(context.m_notarization.is_some());

        let block_hash2 = [65u8; blake3::OUT_LEN];
        let mut votes2 = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash2, leader_id, &setup);
            votes2.insert(vote);
        }
        let m_not2 = create_test_m_notarization::<6, 1, 3>(&votes2, 10, block_hash2, leader_id);

        let result2 = context.add_m_notarization(m_not2, peers);
        assert!(result2.is_ok());

        // Should replace the non-verified M-notarization
        assert!(context.m_notarization.is_some());
        assert_eq!(
            context.m_notarization.as_ref().unwrap().block_hash,
            block_hash1
        );
    }

    #[test]
    fn test_add_m_notarization_non_verified_non_replaced_by_newer() {
        // Test what happens when multiple M-notarizations arrive before block
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [66u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // No block hash set - simulating M-notarizations arriving before block

        // Receive first M-notarization (for block hash 1)
        let block_hash1 = [67u8; blake3::OUT_LEN];
        let mut votes1 = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash1, leader_id, &setup);
            votes1.insert(vote);
        }
        let m_not1 = create_test_m_notarization::<6, 1, 3>(&votes1, 10, block_hash1, leader_id);

        let result1 = context.add_m_notarization(m_not1, peers);
        assert!(result1.is_ok());
        assert!(context.m_notarization.is_some());
        assert_eq!(
            context.m_notarization.as_ref().unwrap().block_hash,
            block_hash1
        );

        // Receive second M-notarization (for block hash 2)
        // If m_notarization.is_none(), it will store
        // Since m_notarization is still None, it WILL replace the first one
        let block_hash2 = [68u8; blake3::OUT_LEN];
        let mut votes2 = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash2, leader_id, &setup);
            votes2.insert(vote);
        }
        let m_not2 = create_test_m_notarization::<6, 1, 3>(&votes2, 10, block_hash2, leader_id);

        let result2 = context.add_m_notarization(m_not2, peers);
        assert!(result2.is_ok());

        assert!(context.m_notarization.is_some());
        assert_eq!(
            context.m_notarization.as_ref().unwrap().block_hash,
            block_hash1 // Should be the first one, no replacement should happen
        );
    }

    #[test]
    fn test_add_m_notarization_then_block_matches() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [67u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Step 1: Create the block first to get its hash
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();

        // Step 2: Create M-notarization for this block hash
        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 10, block_hash, leader_id);

        // Step 3: Receive M-notarization BEFORE block (simulate async network)
        // Don't set block_hash yet to simulate receiving M-notarization first
        let result1 = context.add_m_notarization(m_notarization.clone(), peers);
        assert!(result1.is_ok());

        // Verify M-notarization is stored as non-verified
        assert!(context.m_notarization.is_some());
        assert_eq!(
            context.m_notarization.as_ref().unwrap().block_hash,
            block_hash
        );

        // Step 4: Block arrives - add_new_view_block should move non-verified to verified
        let result2 = context.add_new_view_block(block, peers);
        assert!(result2.is_ok());

        let proposal_result = result2.unwrap();

        // Block hash should be set
        assert_eq!(context.block_hash, Some(block_hash));

        // Non-verified M-notarization should be moved to verified
        assert!(context.m_notarization.is_some());

        // Should detect M-notarization (since we moved it to verified)
        assert!(proposal_result.is_enough_to_m_notarize);

        // Verify the M-notarization matches what we sent
        let verified_m_not = context.m_notarization.as_ref().unwrap();
        assert_eq!(verified_m_not.view, 10);
        assert_eq!(verified_m_not.block_hash, block_hash);
        assert_eq!(verified_m_not.leader_id, leader_id);
    }

    #[test]
    fn test_add_m_notarization_then_block_mismatches() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [68u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Step 1: Receive M-notarization for an initial block hash
        let initial_block_hash = [69u8; blake3::OUT_LEN];
        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, initial_block_hash, leader_id, &setup);
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 10, initial_block_hash, leader_id);

        let result1 = context.add_m_notarization(m_notarization, peers);
        assert!(result1.is_ok());
        assert!(context.m_notarization.is_some());

        // Step 2: Block arrives with DIFFERENT hash
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let actual_block_hash = block.get_hash();
        assert_ne!(actual_block_hash, initial_block_hash); // Verify they're different

        let result2 = context.add_new_view_block(block, peers);
        assert!(result2.is_ok());

        let proposal_result = result2.unwrap();
        assert!(proposal_result.should_nullify);

        // should_nullify should be true
        assert!(proposal_result.should_nullify);
        assert!(!proposal_result.should_vote);
        assert!(!proposal_result.is_enough_to_m_notarize);
        assert!(!proposal_result.is_enough_to_finalize);
        assert!(!proposal_result.should_await);

        // Block hash should be set to the one of the M-notarization block hash
        // as at least one honest replica voted for it, and therefore the leader was Byzantine
        assert!(context.block_hash.is_some());
        assert_eq!(context.block_hash.unwrap(), initial_block_hash);

        // Non-verified M-notarization should NOT be DISCARDED (hash mismatch), but should be
        // nullified
        assert!(context.m_notarization.is_some());
    }

    #[test]
    fn test_add_m_notarization_should_nullify_always_false() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [69u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Even with conflicting evidence, M-notarization should not trigger nullification
        // (the error path handles mismatched block hash)
        context.has_voted = true;
        context.num_invalid_votes = 10; // Lots of conflicting evidence

        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 10, block_hash, leader_id);

        let result = context.add_m_notarization(m_notarization, peers);
        assert!(result.is_ok());

        let notarize_result = result.unwrap();
        // should_nullify should always be false for M-notarization
        // (nullification logic is in add_vote, not add_m_notarization)
        assert!(!notarize_result.should_nullify);
    }

    #[test]
    fn test_add_m_notarization_should_await_always_false() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [70u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Create block first to get its actual hash
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();

        // Create M-notarization with the actual block hash
        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 10, block_hash, leader_id);

        // Without block
        let result1 = context.add_m_notarization(m_notarization.clone(), peers);
        assert!(result1.is_ok());
        assert!(!result1.unwrap().should_await);

        // With block
        context.block_hash = Some(block_hash);
        let result2 = context.add_m_notarization(m_notarization, peers);
        assert!(result2.is_ok());
        assert!(!result2.unwrap().should_await);
    }

    #[test]
    fn test_add_nullify_success() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [35u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        let nullify = create_test_nullify(2, 10, leader_id, &setup);
        let result = context.add_nullify(nullify.clone(), peers);

        assert!(result.is_ok());
        assert!(context.nullify_messages.contains(&nullify));
        assert_eq!(context.nullify_messages.len(), 1);
        assert!(context.nullification.is_none()); // Only 1 message, need >2*F=2
    }

    #[test]
    fn test_add_nullify_creates_nullification_at_threshold() {
        let setup = create_test_peer_setup(6); // N=6, F=1, need 2f+1=3
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [36u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Add nullify messages until threshold
        for i in 1..=3 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            let result = context.add_nullify(nullify, peers);
            assert!(result.is_ok());

            if i < 3 {
                assert!(context.nullification.is_none());
            } else {
                // At i=3, we have 2f+1 messages
                assert!(context.nullification.is_some());
            }
        }

        assert_eq!(context.nullify_messages.len(), 3);
        let nullification = context.nullification.as_ref().unwrap();
        assert_eq!(nullification.view, 10);
        assert_eq!(nullification.leader_id, leader_id);
    }

    #[test]
    fn test_add_nullification_success() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [37u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Create nullification
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            nullify_messages.insert(nullify);
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 10, leader_id);

        let result = context.add_nullification(nullification, peers);
        assert!(result.is_ok());

        let broadcast_result = result.unwrap();
        assert!(broadcast_result.should_broadcast_nullification);
        assert!(context.nullification.is_some());
    }

    #[test]
    fn test_add_nullification_duplicate_does_not_rebroadcast() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [38u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Create nullification
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            nullify_messages.insert(nullify);
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 10, leader_id);

        // Add first time
        let result1 = context.add_nullification(nullification.clone(), peers);
        assert!(result1.unwrap().should_broadcast_nullification);

        // Add second time (should not rebroadcast)
        let result2 = context.add_nullification(nullification, peers);
        assert!(result2.is_ok());
        assert!(!result2.unwrap().should_broadcast_nullification);
    }

    #[test]
    fn test_add_nullify_wrong_view() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [35u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        let nullify = create_test_nullify(2, 11, leader_id, &setup); // Wrong view
        let result = context.add_nullify(nullify, peers);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not the current view")
        );
    }

    #[test]
    fn test_add_nullify_wrong_leader() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let wrong_leader = peers.sorted_peer_ids[1];
        let replica_id = peers.sorted_peer_ids[2];
        let parent_hash = [35u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        let nullify = create_test_nullify(2, 10, wrong_leader, &setup); // Wrong leader
        let result = context.add_nullify(nullify, peers);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not the current leader")
        );
    }

    #[test]
    fn test_add_nullify_invalid_peer() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [35u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        let invalid_peer_id = PeerId::from(999u64);
        let sk = BlsSecretKey::generate(&mut thread_rng());
        let signature = sk.sign(&[10u8; 32]);
        let nullify = Nullify::new(10, leader_id, signature, invalid_peer_id);
        let result = context.add_nullify(nullify, peers);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not present in the peers set")
        );
    }

    #[test]
    fn test_add_nullify_duplicate_peer() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [35u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        let nullify = create_test_nullify(2, 10, leader_id, &setup);

        // Add first time
        let result1 = context.add_nullify(nullify.clone(), peers);
        assert!(result1.is_ok());

        // Add second time (duplicate peer)
        let result2 = context.add_nullify(nullify, peers);
        assert!(result2.is_err());
        assert!(result2.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_add_nullify_invalid_signature() {
        let setup = create_test_peer_setup(4);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [35u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        let peer_id = setup.peer_set.sorted_peer_ids[2];
        let sk = BlsSecretKey::generate(&mut thread_rng());
        let signature = sk.sign(&[10u8; 32]);
        let nullify = Nullify::new(10, leader_id, signature, peer_id);
        let result = context.add_nullify(nullify, peers);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not valid"));
    }

    #[test]
    fn test_add_nullify_finalizes_block_at_threshold() {
        let setup = create_test_peer_setup(6); // N=6, F=1, need 2f+1=3
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [36u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Add a block
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        context.block = Some(block.clone());
        context.block_hash = Some(block.get_hash());

        assert!(!context.block.as_ref().unwrap().is_finalized);

        // Add nullify messages until threshold
        for i in 1..=3 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            let result = context.add_nullify(nullify, peers);
            assert!(result.is_ok());
        }

        // Block should be marked as finalized
        assert!(context.block.as_ref().unwrap().is_finalized);
        assert!(context.nullification.is_some());
    }

    #[test]
    fn test_add_nullify_no_block_to_finalize() {
        let setup = create_test_peer_setup(6); // N=6, F=1, need 2f+1=3
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [36u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // No block added
        assert!(context.block.is_none());

        // Add nullify messages until threshold
        for i in 1..=3 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            let result = context.add_nullify(nullify, peers);
            assert!(result.is_ok());
        }

        // Should create nullification even without block
        assert!(context.nullification.is_some());
        assert!(context.block.is_none());
    }

    #[test]
    fn test_add_nullify_threshold_boundary() {
        let setup = create_test_peer_setup(6); // N=6, F=1, need >2F (>2), so 3 is threshold
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [36u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Add 2 nullifies (exactly 2F, should NOT create nullification)
        for i in 1..=2 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            let result = context.add_nullify(nullify, peers);
            assert!(result.is_ok());
        }
        assert!(context.nullification.is_none());
        assert_eq!(context.nullify_messages.len(), 2);

        // Add 3rd nullify (>2F, should create nullification)
        let nullify = create_test_nullify(3, 10, leader_id, &setup);
        let result = context.add_nullify(nullify, peers);
        assert!(result.is_ok());
        assert!(context.nullification.is_some());
        assert_eq!(context.nullify_messages.len(), 3);
    }

    #[test]
    fn test_add_nullification_wrong_view() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [37u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            let nullify = create_test_nullify(i, 11, leader_id, &setup); // Wrong view
            nullify_messages.insert(nullify);
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 11, leader_id);

        let result = context.add_nullification(nullification, peers);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not the current view")
        );
    }

    #[test]
    fn test_add_nullification_wrong_leader() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let wrong_leader = peers.sorted_peer_ids[1];
        let replica_id = peers.sorted_peer_ids[2];
        let parent_hash = [37u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            let nullify = create_test_nullify(i, 10, wrong_leader, &setup);
            nullify_messages.insert(nullify);
        }
        let nullification =
            create_test_nullification::<6, 1, 3>(&nullify_messages, 10, wrong_leader);

        let result = context.add_nullification(nullification, peers);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not the current leader")
        );
    }

    #[test]
    fn test_add_nullification_invalid_signature() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [37u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Create nullification with invalid signature
        let peer_ids = [
            peers.sorted_peer_ids[1],
            peers.sorted_peer_ids[2],
            peers.sorted_peer_ids[3],
        ];
        let sk = BlsSecretKey::generate(&mut thread_rng());
        let signature = sk.sign(&[10u8; 32]);
        let nullification = Nullification::new(10, leader_id, signature, peer_ids);

        let result = context.add_nullification(nullification, peers);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not valid"));
    }

    #[test]
    fn test_add_nullification_finalizes_block() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [37u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Add a block
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        context.block = Some(block.clone());
        context.block_hash = Some(block.get_hash());

        assert!(!context.block.as_ref().unwrap().is_finalized);

        // Create and add nullification
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            nullify_messages.insert(nullify);
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 10, leader_id);

        let result = context.add_nullification(nullification, peers);
        assert!(result.is_ok());

        // Block should be marked as finalized
        assert!(context.block.as_ref().unwrap().is_finalized);
    }

    #[test]
    fn test_add_nullification_no_block() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [37u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // No block
        assert!(context.block.is_none());

        // Create and add nullification
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            nullify_messages.insert(nullify);
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 10, leader_id);

        let result = context.add_nullification(nullification, peers);
        assert!(result.is_ok());

        // Should succeed even without block
        assert!(context.nullification.is_some());
    }

    #[test]
    fn test_add_nullification_after_add_nullify() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [38u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Add individual nullify messages first
        for i in 1..=2 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            context.add_nullify(nullify, peers).unwrap();
        }
        assert!(context.nullification.is_none());
        assert_eq!(context.nullify_messages.len(), 2);

        // Now receive a complete nullification from another peer
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            nullify_messages.insert(nullify);
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 10, leader_id);

        let result = context.add_nullification(nullification, peers);
        assert!(result.is_ok());
        assert!(result.unwrap().should_broadcast_nullification);

        // Should have nullification now
        assert!(context.nullification.is_some());
        // Original nullify messages should remain
        assert_eq!(context.nullify_messages.len(), 2);
    }

    #[test]
    fn test_should_timeout_nullify_before_timeout() {
        let setup = create_test_peer_setup(4);
        let peers = setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [39u8; blake3::OUT_LEN];
        let context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Check immediately - should not timeout
        assert!(!context.should_timeout_nullify(Duration::from_secs(10)));
    }

    #[test]
    fn test_should_timeout_nullify_after_timeout() {
        let setup = create_test_peer_setup(4);
        let peers = setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [40u8; blake3::OUT_LEN];
        let context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Check with 0 timeout - should timeout immediately
        assert!(context.should_timeout_nullify(Duration::from_secs(0)));
    }

    #[test]
    fn test_should_timeout_nullify_not_if_already_voted() {
        let setup = create_test_peer_setup(4);
        let peers = setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [41u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Mark as voted
        context.has_voted = true;

        // Should NOT timeout if already voted (per Minimmit Algorithm 1, line 12)
        assert!(!context.should_timeout_nullify(Duration::from_secs(0)));
    }

    #[test]
    fn test_should_timeout_nullify_not_if_already_nullified() {
        let setup = create_test_peer_setup(4);
        let peers = setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [42u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Mark as nullified
        context.has_nullified = true;

        // Should NOT timeout if already nullified
        assert!(!context.should_timeout_nullify(Duration::from_secs(0)));
    }

    #[test]
    fn test_create_nullify_for_timeout_success() {
        let setup = create_test_peer_setup(4);
        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let parent_hash = [43u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        let secret_key = setup.peer_id_to_secret_key.get(&replica_id).unwrap();
        let result = context.create_nullify_for_timeout(secret_key);

        assert!(result.is_ok());
        let nullify = result.unwrap();
        assert_eq!(nullify.view, 10);
        assert_eq!(nullify.leader_id, leader_id);
        assert_eq!(nullify.peer_id, replica_id);
        assert!(context.has_nullified); // Should mark as nullified
    }

    #[test]
    fn test_create_nullify_for_timeout_fails_if_already_nullified() {
        let setup = create_test_peer_setup(4);
        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let parent_hash = [44u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        context.has_nullified = true;

        let secret_key = setup.peer_id_to_secret_key.get(&replica_id).unwrap();
        let result = context.create_nullify_for_timeout(secret_key);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Already nullified in this view")
        );
    }

    #[test]
    fn test_create_nullify_for_timeout_fails_if_already_voted() {
        let setup = create_test_peer_setup(4);
        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let parent_hash = [45u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        context.has_voted = true;

        let secret_key = setup.peer_id_to_secret_key.get(&replica_id).unwrap();
        let result = context.create_nullify_for_timeout(secret_key);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Cannot nullify for timeout after voting - use create_nullify_for_byzantine if conflicting evidence detected")
        );
    }

    #[test]
    fn test_should_nullify_with_invalid_votes() {
        let setup = create_test_peer_setup(6); // F=1, need >2 conflicting
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [46u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Mark as voted
        context.has_voted = true;

        // Add invalid votes (not nullify messages)
        context.num_invalid_votes = 3; // >2f+1

        assert!(context.should_nullify_after_receiving_new_vote());
    }

    #[test]
    fn test_should_nullify_with_nullify_messages() {
        let setup = create_test_peer_setup(6); // F=1, need >2 conflicting
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [47u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Mark as voted
        context.has_voted = true;

        // Add nullify messages
        for i in 0..3 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            context.nullify_messages.insert(nullify);
        }

        assert!(context.should_nullify_after_receiving_new_vote());
    }

    #[test]
    fn test_should_nullify_with_mixed_evidence() {
        let setup = create_test_peer_setup(6); // F=1, need >2 conflicting
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [48u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Mark as voted
        context.has_voted = true;

        // Add 2 invalid votes + 1 nullify message = 3 total (>2f+1)
        context.num_invalid_votes = 2;
        let nullify = create_test_nullify(0, 10, leader_id, &setup);
        context.nullify_messages.insert(nullify);

        assert!(context.should_nullify_after_receiving_new_vote());
    }

    #[test]
    fn test_should_not_nullify_if_not_voted() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [49u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // NOT voted
        assert!(!context.has_voted);

        // Add conflicting evidence
        context.num_invalid_votes = 2;

        // Should nullify if there is conflicting evidence, even if not voted yet
        assert!(context.should_nullify_after_receiving_new_vote());
    }

    #[test]
    fn test_should_not_nullify_if_already_nullified() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [50u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        context.has_voted = true;
        context.has_nullified = true;

        // Add conflicting evidence
        context.num_invalid_votes = 3;

        // Should NOT nullify if already nullified
        assert!(!context.should_nullify_after_receiving_new_vote());
    }

    #[test]
    fn test_should_not_nullify_below_threshold() {
        let setup = create_test_peer_setup(6); // F=1, need >2
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [51u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        context.has_voted = true;

        // Only 1 conflicting (need > F)
        context.num_invalid_votes = 2;

        assert!(context.should_nullify_after_receiving_new_vote());
    }

    #[test]
    fn test_full_voting_flow_to_m_notarization() {
        // Test complete flow: add block -> add votes -> reach M-notarization
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [52u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // 1. Add block proposal
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        let proposal_result = context.add_new_view_block(block, peers).unwrap();
        assert!(proposal_result.should_vote);
        assert!(!proposal_result.is_enough_to_m_notarize);

        // 2. Add votes until M-notarization
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            let vote_result = context.add_vote(vote, peers).unwrap();

            if i < 2 {
                assert!(!vote_result.is_enough_to_m_notarize);
            } else if i == 2 {
                assert!(vote_result.is_enough_to_m_notarize);
            } else {
                assert!(!vote_result.is_enough_to_finalize);
            }
        }

        // 3. Verify M-notarization was created
        assert!(context.m_notarization.is_some());
        assert_eq!(context.votes.len(), 4);
    }

    #[test]
    fn test_full_nullification_flow() {
        // Test complete flow: add nullify messages -> reach nullification
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [53u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Add nullify messages until nullification
        for i in 1..=3 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            context.add_nullify(nullify, peers).unwrap();
        }

        // Should have created nullification
        assert!(context.nullification.is_some());
        assert_eq!(context.nullify_messages.len(), 3);
    }

    #[test]
    fn test_integration_mixed_valid_invalid_votes() {
        // Test: Receive block, add valid votes, then invalid votes trigger nullification
        let setup = create_test_peer_setup(7); // Need 7 peers to have enough distinct voters
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [54u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // 1. Receive block and vote for it
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.add_new_view_block(block, peers).unwrap();

        // Simulate own vote
        context.has_voted = true;

        // 2. Add 2 valid votes (matching block hash) from peers 2 and 3
        for i in 2..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            let result = context.add_vote(vote, peers).unwrap();
            assert!(!result.should_nullify);
        }
        assert_eq!(context.votes.len(), 3);
        assert_eq!(context.num_invalid_votes, 0);

        // 3. Add 3 invalid votes (different block hash) from peers 4, 5, 6 - should trigger
        //    nullification
        let wrong_hash = [99u8; blake3::OUT_LEN];
        for i in 4..=6 {
            let wrong_vote = create_test_vote(i, 10, wrong_hash, leader_id, &setup);
            let result = context.add_vote(wrong_vote, peers).unwrap();

            if i == 6 {
                // At 3rd invalid vote, should detect conflicting evidence
                assert!(result.should_nullify);
            }
        }

        assert_eq!(context.num_invalid_votes, 3);
        assert!(context.should_nullify_after_receiving_new_vote());
    }

    #[test]
    fn test_integration_nullify_and_valid_votes_conflict() {
        // Test: Mix of nullify messages and valid votes, causing conflicting evidence
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [55u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // 1. Receive block and vote
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.add_new_view_block(block, peers).unwrap();
        context.has_voted = true;

        // 2. Add 1 valid vote
        let vote = create_test_vote(2, 10, block_hash, leader_id, &setup);
        context.add_vote(vote, peers).unwrap();
        assert_eq!(context.votes.len(), 2);

        // 3. Add nullify messages (conflicting with our vote)
        for i in 3..=5 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            context.add_nullify(nullify, peers).unwrap();
        }

        // With 3 nullify messages, should detect conflicting evidence
        assert_eq!(context.nullify_messages.len(), 3);
        // A nullification should have been created automatically
        assert!(context.nullification.is_some());
        // And the has_nullified flag should be set
        assert!(context.has_nullified);
    }

    #[test]
    fn test_integration_m_notarization_arrives_before_block() {
        // Test: M-notarization arrives before block, stored as non-verified, then block arrives
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [56u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // 1. Create and send M-notarization before block
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();

        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 10, block_hash, leader_id);

        // Add M-notarization first (no block yet)
        let result1 = context
            .add_m_notarization(m_notarization.clone(), peers)
            .unwrap();
        assert!(!result1.should_notarize);
        assert!(context.m_notarization.is_some());

        // 2. Now block arrives - should process non-verified M-notarization
        let result2 = context.add_new_view_block(block, peers).unwrap();
        assert!(result2.should_vote);
        assert!(result2.is_enough_to_m_notarize); // Should detect M-notarization
        assert!(context.m_notarization.is_some()); // Promoted to verified
    }

    #[test]
    fn test_integration_votes_arrive_before_block() {
        // Test: Non-verified votes arrive before block proposal
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [57u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // 1. Create block (but don't add to context yet)
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();

        // 2. Add votes before block (stored as non-verified)
        for i in 1..=2 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            let result = context.add_vote(vote, peers).unwrap();
            assert!(!result.is_enough_to_m_notarize);
        }
        assert_eq!(context.non_verified_votes.len(), 2);
        assert_eq!(context.votes.len(), 0);

        // 3. Block arrives - should process non-verified votes
        let result = context.add_new_view_block(block, peers).unwrap();
        assert!(result.should_vote);
        assert_eq!(context.votes.len(), 3); // Non-verified votes promoted
        assert_eq!(context.non_verified_votes.len(), 0);

        // 4. One more vote should trigger M-notarization
        let vote3 = create_test_vote(3, 10, block_hash, leader_id, &setup);
        let result = context.add_vote(vote3, peers).unwrap();
        assert!(result.is_enough_to_m_notarize);
        assert!(context.m_notarization.is_some());
    }

    #[test]
    fn test_integration_concurrent_m_notarization_and_nullification() {
        // Test: Both M-notarization and nullification can coexist (network partition scenario)
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [58u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // 1. Add block
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.add_new_view_block(block, peers).unwrap();

        // 2. Collect 3 votes for M-notarization
        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            votes.insert(vote.clone());
            context.add_vote(vote, peers).unwrap();
        }
        assert!(context.m_notarization.is_some());

        // 3. Also collect 3 nullify messages (different set of replicas)
        for i in 3..=5 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            context.add_nullify(nullify, peers).unwrap();
        }

        // Both can coexist - indicates network partition or Byzantine behavior
        assert!(context.m_notarization.is_some());
        assert!(context.nullification.is_some());
        assert_eq!(context.votes.len(), 4);
        assert_eq!(context.nullify_messages.len(), 3);
    }

    #[test]
    fn test_integration_full_l_notarization_flow() {
        // Test: Complete flow to L-notarization (n-f votes)
        let setup = create_test_peer_setup(6); // N=6, F=1, need n-f=5 votes for L-notarization
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [59u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // 1. Add block
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        let proposal_result = context.add_new_view_block(block, peers).unwrap();
        assert!(proposal_result.should_vote);

        // 2. Add votes progressively to M-notarization then L-notarization
        for i in 1..=4 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            let vote_result = context.add_vote(vote, peers).unwrap();

            if i < 2 {
                assert!(!vote_result.is_enough_to_m_notarize);
                assert!(!vote_result.is_enough_to_finalize);
            } else if i == 2 {
                // M-notarization threshold
                assert!(vote_result.is_enough_to_m_notarize);
                assert!(!vote_result.is_enough_to_finalize);
                assert!(context.m_notarization.is_some());
            } else if i < 4 {
                assert!(!vote_result.is_enough_to_finalize);
            } else {
                // L-notarization threshold (i == 5)
                assert!(vote_result.is_enough_to_finalize);
            }
        }

        assert_eq!(context.votes.len(), 5);
        assert!(context.m_notarization.is_some());
    }

    #[test]
    fn test_integration_mixed_votes_with_non_verified_m_notarization() {
        // Test: Non-verified votes + non-verified M-notarization + block arrival
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [60u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();

        // 1. Add non-verified votes (block hasn't arrived)
        for i in 1..=2 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            context.add_vote(vote, peers).unwrap();
        }
        assert_eq!(context.non_verified_votes.len(), 2);

        // 2. Add non-verified M-notarization
        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 10, block_hash, leader_id);
        context.add_m_notarization(m_notarization, peers).unwrap();
        assert!(context.m_notarization.is_some());

        // 3. Block arrives - should process both non-verified votes and M-notarization
        let result = context.add_new_view_block(block, peers).unwrap();
        assert!(result.should_vote);
        assert!(result.is_enough_to_m_notarize);

        assert_eq!(context.votes.len(), 3); // Non-verified votes promoted
        assert!(context.m_notarization.is_some()); // M-notarization promoted
        assert_eq!(context.non_verified_votes.len(), 0);
    }

    #[test]
    fn test_integration_invalid_votes_then_valid_m_notarization() {
        // Test: Accumulate invalid votes, then valid M-notarization arrives
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [61u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // 1. Add block
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.add_new_view_block(block, peers).unwrap();

        // 2. Add 2 invalid votes (different block hash)
        let wrong_hash = [99u8; blake3::OUT_LEN];
        for i in 1..=2 {
            let wrong_vote = create_test_vote(i, 10, wrong_hash, leader_id, &setup);
            context.add_vote(wrong_vote, peers).unwrap();
        }
        assert_eq!(context.num_invalid_votes, 2);
        assert_eq!(context.votes.len(), 1);

        // 3. Valid M-notarization arrives (for correct block)
        let mut valid_votes = HashSet::new();
        for i in 3..=5 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            valid_votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&valid_votes, 10, block_hash, leader_id);
        let result = context.add_m_notarization(m_notarization, peers).unwrap();

        assert!(result.should_notarize);
        assert!(context.m_notarization.is_some());
        assert_eq!(context.num_invalid_votes, 2); // Invalid votes remain tracked
    }

    #[test]
    fn test_integration_voted_then_timeout_cannot_nullify() {
        // Test: After voting, timeout should not trigger nullification
        let setup = create_test_peer_setup(7);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [62u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // 1. Add block and vote
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        context.add_new_view_block(block, peers).unwrap();
        context.has_voted = true;

        // 2. Add 2 valid votes
        let block_hash = context.block_hash.unwrap();
        for i in 2..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            context.add_vote(vote, peers).unwrap();
        }

        // 3. Timeout occurs - should NOT nullify (already voted)
        assert!(!context.should_timeout_nullify(Duration::from_secs(0)));

        // 4. But conflicting evidence (invalid votes) SHOULD trigger nullification
        let wrong_hash = [99u8; blake3::OUT_LEN];
        for i in 4..=6 {
            let wrong_vote = create_test_vote(i, 10, wrong_hash, leader_id, &setup);
            context.add_vote(wrong_vote, peers).unwrap();
        }

        // Now should nullify due to conflicting evidence (not timeout)
        assert!(context.should_nullify_after_receiving_new_vote());
    }

    #[test]
    fn test_integration_complete_byzantine_detection() {
        // Test: Detect Byzantine behavior with mixed valid/invalid messages
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [63u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // 1. Add block and vote
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.add_new_view_block(block, peers).unwrap();
        context.has_voted = true;

        // 2. Add 2 valid votes
        for i in 2..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            context.add_vote(vote, peers).unwrap();
        }

        // 3. Add 1 invalid vote + 2 nullify messages (total 3 conflicting)
        let wrong_hash = [99u8; blake3::OUT_LEN];
        let wrong_vote = create_test_vote(4, 10, wrong_hash, leader_id, &setup);
        context.add_vote(wrong_vote, peers).unwrap();

        for i in 4..=5 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            context.add_nullify(nullify, peers).unwrap();
        }

        // Should detect conflicting evidence: 1 invalid vote + 2 nullify = 3 > 2F
        assert_eq!(context.num_invalid_votes, 1);
        assert_eq!(context.nullify_messages.len(), 2);
        assert!(context.should_nullify_after_receiving_new_vote());
    }

    #[test]
    fn test_add_m_notarization_should_forward_for_new_m_notarization() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [100u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Create M-notarization
        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 10, block_hash, leader_id);

        // First M-notarization should have should_forward = true
        let result = context.add_m_notarization(m_notarization, peers);
        assert!(result.is_ok());
        assert!(result.unwrap().should_forward);
    }

    #[test]
    fn test_add_m_notarization_should_not_forward_for_duplicate() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [101u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        context.block_hash = Some(block_hash);

        // Create M-notarization
        let mut votes = HashSet::new();
        for i in 1..=3 {
            let vote = create_test_vote(i, 10, block_hash, leader_id, &setup);
            votes.insert(vote);
        }
        let m_notarization =
            create_test_m_notarization::<6, 1, 3>(&votes, 10, block_hash, leader_id);

        // Add first time
        context
            .add_m_notarization(m_notarization.clone(), peers)
            .unwrap();

        // Add second time (duplicate) - should have should_forward = false
        let result = context.add_m_notarization(m_notarization, peers);
        assert!(result.is_ok());
        assert!(!result.unwrap().should_forward);
    }

    #[test]
    fn test_add_nullification_should_broadcast_for_new_nullification() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [102u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Create nullification
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            nullify_messages.insert(nullify);
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 10, leader_id);

        // First nullification should have should_broadcast_nullification = true
        let result = context.add_nullification(nullification, peers);
        assert!(result.is_ok());
        assert!(result.unwrap().should_broadcast_nullification);
    }

    #[test]
    fn test_add_nullification_should_not_broadcast_for_duplicate() {
        let setup = create_test_peer_setup(6);
        let peers = &setup.peer_set;
        let leader_id = peers.sorted_peer_ids[0];
        let replica_id = peers.sorted_peer_ids[1];
        let parent_hash = [103u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Create nullification
        let mut nullify_messages = HashSet::new();
        for i in 1..=3 {
            let nullify = create_test_nullify(i, 10, leader_id, &setup);
            nullify_messages.insert(nullify);
        }
        let nullification = create_test_nullification::<6, 1, 3>(&nullify_messages, 10, leader_id);

        // Add first time
        context
            .add_nullification(nullification.clone(), peers)
            .unwrap();

        // Add second time (duplicate) - should have should_broadcast_nullification = false
        let result = context.add_nullification(nullification, peers);
        assert!(result.is_ok());
        assert!(!result.unwrap().should_broadcast_nullification);
    }

    #[test]
    fn test_create_nullify_for_byzantine_success_before_voting() {
        // Test: Byzantine nullification can happen BEFORE voting if conflicting evidence detected
        let setup = create_test_peer_setup(4);
        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let parent_hash = [0u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Replica has NOT voted yet
        assert!(!context.has_voted);

        let secret_key = setup.peer_id_to_secret_key.get(&replica_id).unwrap();
        let result = context.create_nullify_for_byzantine(secret_key);

        assert!(result.is_ok());
        let nullify = result.unwrap();
        assert_eq!(nullify.view, 10);
        assert_eq!(nullify.leader_id, leader_id);
        assert!(context.has_nullified);
    }

    #[test]
    fn test_create_nullify_for_byzantine_success_after_voting() {
        // Test: Byzantine nullification can happen AFTER voting if conflicting evidence detected
        let setup = create_test_peer_setup(4);
        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let parent_hash = [0u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Replica HAS voted
        context.has_voted = true;

        let secret_key = setup.peer_id_to_secret_key.get(&replica_id).unwrap();
        let result = context.create_nullify_for_byzantine(secret_key);

        assert!(result.is_ok());
        let nullify = result.unwrap();
        assert_eq!(nullify.view, 10);
        assert_eq!(nullify.leader_id, leader_id);
        assert!(context.has_nullified);
    }

    #[test]
    fn test_create_nullify_for_byzantine_fails_if_already_nullified() {
        let setup = create_test_peer_setup(4);
        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let parent_hash = [0u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        context.has_nullified = true;

        let secret_key = setup.peer_id_to_secret_key.get(&replica_id).unwrap();
        let result = context.create_nullify_for_byzantine(secret_key);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Already nullified")
        );
    }

    #[test]
    fn test_timeout_nullify_vs_byzantine_nullify_distinction() {
        // Test: Timeout nullify requires !has_voted, Byzantine nullify allows any state
        let setup = create_test_peer_setup(4);
        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let parent_hash = [0u8; blake3::OUT_LEN];
        let secret_key = setup.peer_id_to_secret_key.get(&replica_id).unwrap();

        // Scenario 1: Before voting - both methods should work
        let mut context1 = create_test_view_context(10, leader_id, replica_id, parent_hash);
        assert!(context1.create_nullify_for_timeout(secret_key).is_ok());

        let mut context2 = create_test_view_context(10, leader_id, replica_id, parent_hash);
        assert!(context2.create_nullify_for_byzantine(secret_key).is_ok());

        // Scenario 2: After voting - only Byzantine method should work
        let mut context3 = create_test_view_context(10, leader_id, replica_id, parent_hash);
        context3.has_voted = true;
        assert!(context3.create_nullify_for_timeout(secret_key).is_err());

        let mut context4 = create_test_view_context(10, leader_id, replica_id, parent_hash);
        context4.has_voted = true;
        assert!(context4.create_nullify_for_byzantine(secret_key).is_ok());
    }

    #[test]
    fn test_integration_byzantine_detection_before_voting() {
        // Integration test: Replica receives conflicting votes before voting, should use Byzantine
        // nullify
        let setup = create_test_peer_setup(6); // N=6, F=1, need >2 conflicting
        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let parent_hash = [0u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Add a block
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 10);
        context.add_new_view_block(block, &setup.peer_set).unwrap();

        // Replica has NOT voted yet
        assert!(!context.has_voted);

        // Receive 3 votes for DIFFERENT blocks (Byzantine behavior)
        let wrong_hash = [1u8; blake3::OUT_LEN];
        for i in 2..=4 {
            let vote = create_test_vote(i, 10, wrong_hash, leader_id, &setup);
            context.add_vote(vote, &setup.peer_set).unwrap();
        }

        // Should have 3 invalid votes
        assert_eq!(context.num_invalid_votes, 3);

        // Replica hasn't voted but detected Byzantine behavior
        assert!(!context.has_voted);

        // Byzantine nullification should work (even before voting)
        let secret_key = setup.peer_id_to_secret_key.get(&replica_id).unwrap();
        let result = context.create_nullify_for_byzantine(secret_key);
        assert!(result.is_ok());
    }

    #[test]
    fn test_integration_byzantine_detection_after_voting() {
        // Integration test: Replica votes, then receives conflicting evidence, should use Byzantine
        // nullify
        let setup = create_test_peer_setup(6); // N=6, F=1, need >2 conflicting
        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let parent_hash = [0u8; blake3::OUT_LEN];
        let mut context =
            create_test_view_context_with_params::<6, 1, 3>(10, leader_id, replica_id, parent_hash);

        // Add a block
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap();
        let block = create_test_block(10, leader_id, parent_hash, leader_sk.clone(), 10);
        let block_hash = block.get_hash();
        context.add_new_view_block(block, &setup.peer_set).unwrap();

        // Replica votes for the block
        let replica_sk = setup.peer_id_to_secret_key.get(&replica_id).unwrap();
        let vote_sig = replica_sk.sign(&block_hash);
        context.add_own_vote(block_hash, vote_sig).unwrap();
        assert!(context.has_voted);

        // Then receive 3 votes for a DIFFERENT block (Byzantine behavior)
        let wrong_hash = [1u8; blake3::OUT_LEN];
        for i in 2..=4 {
            let vote = create_test_vote(i, 10, wrong_hash, leader_id, &setup);
            context.add_vote(vote, &setup.peer_set).unwrap();
        }

        // Should have 3 invalid votes
        assert_eq!(context.num_invalid_votes, 3);

        // Replica has voted and detected Byzantine behavior
        assert!(context.has_voted);

        // Byzantine nullification should work (after voting)
        let result = context.create_nullify_for_byzantine(replica_sk);
        assert!(result.is_ok());
    }

    #[test]
    fn test_timeout_nullification_scenario() {
        // Test: Timeout nullification only works before voting and without Byzantine evidence
        let setup = create_test_peer_setup(4);
        let leader_id = setup.peer_set.sorted_peer_ids[0];
        let replica_id = setup.peer_set.sorted_peer_ids[1];
        let parent_hash = [0u8; blake3::OUT_LEN];
        let mut context = create_test_view_context(10, leader_id, replica_id, parent_hash);

        // Replica hasn't voted, no block received, no conflicting evidence
        assert!(!context.has_voted);
        assert!(context.block.is_none());
        assert_eq!(context.num_invalid_votes, 0);
        assert_eq!(context.nullify_messages.len(), 0);

        // Timeout nullification should work
        let secret_key = setup.peer_id_to_secret_key.get(&replica_id).unwrap();
        let result = context.create_nullify_for_timeout(secret_key);
        assert!(result.is_ok());
        assert!(context.has_nullified);
    }

    #[test]
    fn test_add_m_notarization_sets_block_hash_if_missing() {
        let setup = create_test_peer_setup(4);
        // N=4, F=1, M=3
        let mut ctx = create_test_view_context(
            1,
            setup.peer_set.sorted_peer_ids[0],
            setup.peer_set.sorted_peer_ids[1],
            [0u8; 32],
        );

        // Simulate receiving 3 votes (M-notarization threshold) for a block hash
        let block_hash = [0xaa; 32];
        let mut votes = HashSet::new();
        for i in 0..3 {
            let vote =
                create_test_vote(i, 1, block_hash, setup.peer_set.sorted_peer_ids[0], &setup);
            votes.insert(vote);
        }

        let m_notarization =
            create_test_m_notarization(&votes, 1, block_hash, setup.peer_set.sorted_peer_ids[0]);

        // Pre-condition: Block hash is unknown
        assert!(ctx.block_hash.is_none());

        // Action: Add M-notarization
        let result = ctx.add_m_notarization(m_notarization, &setup.peer_set);

        // Assertion: Result is Ok and block_hash is now set
        assert!(result.is_ok());
        assert_eq!(ctx.block_hash, Some(block_hash));
        assert!(ctx.m_notarization.is_some());
    }

    #[test]
    fn test_add_m_notarization_processes_non_verified_votes() {
        let setup = create_test_peer_setup(4);
        let mut ctx = create_test_view_context(
            1,
            setup.peer_set.sorted_peer_ids[0],
            setup.peer_set.sorted_peer_ids[1],
            [0u8; 32],
        );
        let block_hash = [0xaa; 32];

        // Add a vote to non-verified (because block hash is unknown)
        let vote = create_test_vote(2, 1, block_hash, setup.peer_set.sorted_peer_ids[0], &setup);
        ctx.add_vote(vote.clone(), &setup.peer_set).unwrap();
        assert!(ctx.non_verified_votes.contains(&vote));
        assert!(ctx.votes.is_empty());

        // Create M-notarization
        let mut votes = HashSet::new();
        for i in 0..3 {
            votes.insert(create_test_vote(
                i,
                1,
                block_hash,
                setup.peer_set.sorted_peer_ids[0],
                &setup,
            ));
        }
        let m_notarization =
            create_test_m_notarization(&votes, 1, block_hash, setup.peer_set.sorted_peer_ids[0]);

        // Add M-notarization
        ctx.add_m_notarization(m_notarization, &setup.peer_set)
            .unwrap();

        // Assertion: The vote should move from non-verified to valid votes
        assert!(ctx.non_verified_votes.is_empty());
        assert!(ctx.votes.contains(&vote));
    }

    #[test]
    fn test_state_diff_field_is_none_on_new_context() {
        let leader_id = 12345u64;
        let replica_id = 67890u64;
        let parent_hash = [0u8; blake3::OUT_LEN];

        let ctx: ViewContext<5, 1, 3> = ViewContext::new(1, leader_id, replica_id, parent_hash);

        assert!(
            ctx.state_diff.is_none(),
            "state_diff should be None on new ViewContext"
        );
    }

    #[test]
    fn test_state_diff_can_be_assigned_arc() {
        let leader_id = 12345u64;
        let replica_id = 67890u64;
        let parent_hash = [0u8; blake3::OUT_LEN];

        let mut ctx: ViewContext<5, 1, 3> = ViewContext::new(1, leader_id, replica_id, parent_hash);

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        let mut diff = crate::validation::types::StateDiff::new();
        diff.add_created_account(addr, 5000);

        let diff_arc = Arc::new(diff);
        ctx.state_diff = Some(Arc::clone(&diff_arc));

        assert!(ctx.state_diff.is_some());

        // Arc should now have 2 strong references
        assert_eq!(Arc::strong_count(&diff_arc), 2);
    }

    #[test]
    fn test_state_diff_is_independent_of_block() {
        // state_diff can be set regardless of whether block is set
        let mut rng = thread_rng();
        let sk = BlsSecretKey::generate(&mut rng);
        let pk = sk.public_key();
        let leader_id = pk.to_peer_id();
        let replica_id = 67890u64;
        let parent_hash = [0u8; blake3::OUT_LEN];

        let mut ctx: ViewContext<5, 1, 3> = ViewContext::new(1, leader_id, replica_id, parent_hash);

        // No block set
        assert!(ctx.block.is_none());

        // But we can set state_diff
        let diff = crate::validation::types::StateDiff::new();
        ctx.state_diff = Some(Arc::new(diff));

        assert!(ctx.state_diff.is_some());
        assert!(ctx.block.is_none()); // Still no block
    }

    #[test]
    fn test_state_diff_persists_across_vote_additions() {
        // Verify state_diff is not affected by adding votes
        let mut rng = thread_rng();

        // Generate keypairs for peers
        let mut public_keys = vec![];
        let mut peer_id_to_secret_key = HashMap::new();
        for _ in 0..5 {
            let sk = BlsSecretKey::generate(&mut rng);
            let pk = sk.public_key();
            let peer_id = pk.to_peer_id();
            peer_id_to_secret_key.insert(peer_id, sk);
            public_keys.push(pk);
        }
        let peer_set = crate::state::peer::PeerSet::new(public_keys);

        let leader_id = peer_set.sorted_peer_ids[0];
        let leader_sk = peer_id_to_secret_key.get(&leader_id).unwrap();
        let replica_id = peer_set.sorted_peer_ids[1];
        let parent_hash = [0u8; blake3::OUT_LEN];

        let mut ctx: ViewContext<5, 1, 3> = ViewContext::new(1, leader_id, replica_id, parent_hash);

        // Add block first
        let block = create_test_block(1, leader_id, parent_hash, leader_sk.clone(), 1);
        let block_hash = block.get_hash();
        ctx.add_new_view_block(block, &peer_set).unwrap();

        // Set state_diff
        let diff = crate::validation::types::StateDiff::new();
        ctx.state_diff = Some(Arc::new(diff));

        // Add votes
        for i in 1..3 {
            let peer_id = peer_set.sorted_peer_ids[i];
            let peer_sk = peer_id_to_secret_key.get(&peer_id).unwrap();
            let signature = peer_sk.sign(&block_hash);
            let vote = crate::state::notarizations::Vote::new(
                1, block_hash, signature, peer_id, leader_id,
            );
            ctx.add_vote(vote, &peer_set).unwrap();
        }

        // state_diff should still be set
        assert!(
            ctx.state_diff.is_some(),
            "state_diff should persist after adding votes"
        );
    }
}
