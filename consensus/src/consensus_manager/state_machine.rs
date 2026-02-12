//! Consensus State Machine - Core Protocol Logic for Minimmit BFT
//!
//! This module implements the [`ConsensusStateMachine`], which contains the core logic
//! for executing the Minimmit Byzantine Fault Tolerant (BFT) consensus protocol. The state
//! machine runs in a dedicated thread (spawned by [`ConsensusEngine`]) and processes
//! consensus messages, transactions, and timer events to drive view progression and
//! maintain protocol invariants.
//!
//! ## Architecture
//!
//! The state machine follows an event-driven architecture, continuously polling lock-free
//! ring buffers for incoming messages and periodically triggering timeout events:
//!
//!
//! ┌────────────────────────────────────────────────────────────────┐
//! │              ConsensusStateMachine (Event Loop)                │
//! │                                                                │
//! │  ┌──────────────────────────────────────────────────────────┐  │
//! │  │  1. Poll message_consumer (ConsensusMessages)            │  │
//! │  │     - BlockProposal (validated synchronously via         │  │
//! │  │       ViewProgressManager), Vote, MNotarization,         │  │
//! │  │       LNotarization, Nullify, Nullification              │  │
//! │  └────────────────────┬─────────────────────────────────────┘  │
//! │                       │                                        │
//! │                       ▼                                        │
//! │  ┌──────────────────────────────────────────────────────────┐  │
//! │  │  2. Periodic Tick (every tick_interval)                  │  │
//! │  │     - Check view timeout                                 │  │
//! │  │     - Trigger nullification if needed                    │  │
//! │  └────────────────────┬─────────────────────────────────────┘  │
//! │                       │                                        │
//! │                       ▼                                        │
//! │  ┌──────────────────────────────────────────────────────────┐  │
//! │  │  3. Process Event (ViewProgressEvent)                    │  │
//! │  │     - ViewProgressManager returns events based on state  │  │
//! │  └────────────────────┬─────────────────────────────────────┘  │
//! │                       │                                        │
//! │                       ▼                                        │
//! │  ┌──────────────────────────────────────────────────────────┐  │
//! │  │  4. Handle Event (take action)                           │  │
//! │  │     - Propose block, vote, create notarizations          │  │
//! │  │     - Broadcast messages, progress views                 │  │
//! │  └────────────────────┬─────────────────────────────────────┘  │
//! │                       │                                        │
//! │                       ▼                                        │
//! │  ┌──────────────────────────────────────────────────────────┐  │
//! │  │  5. Broadcast via broadcast_producer                     │  │
//! │  │     - Send messages to network layer                     │  │
//! │  └──────────────────────────────────────────────────────────┘  │
//! │                                                                │
//! │  Loop continues until shutdown_signal is set                   │
//! └────────────────────────────────────────────────────────────────┘
//!
//! ## Responsibilities
//!
//! The `ConsensusStateMachine` is responsible for:
//!
//! - **Message Processing**: Consuming and validating incoming consensus messages (block proposals,
//!   votes, M-notarizations, L-notarizations, nullifications)
//! - **Validated Block Processing**: Receiving validated blocks with their `StateDiff` from the
//!   validation service and integrating them into the consensus flow
//! - **View Progression**: Advancing through views based on M-notarizations or nullifications
//! - **Leader Duties**: Proposing blocks when this replica is the leader
//! - **Voting**: Casting votes for valid block proposals
//! - **Notarization**: Creating and broadcasting M-notarizations (2F+1 votes) and L-notarizations
//!   (N-F votes)
//! - **Nullification**: Detecting Byzantine behavior or timeouts and creating nullify messages
//! - **Message Broadcasting**: Forwarding messages to other replicas exactly once (per protocol)
//! - **Cryptographic Operations**: Signing blocks, votes, and nullifications with BLS signatures
//! - **State Persistence**: Delegating to `ViewProgressManager` for durable state
//!
//! ## Minimmit Protocol Overview
//!
//! The Minimmit protocol organizes consensus into numbered **views**, each with a designated
//! **leader** (determined by round-robin or other strategies). The protocol proceeds as follows:
//!
//! 1. **Block Proposal**: The leader for view V proposes a block extending the most recent
//!    M-notarized block
//! 2. **Voting**: Replicas vote for the leader's block if it's valid
//! 3. **M-Notarization** (2F+1 votes): Once 2F+1 votes are collected, an M-notarization is created,
//!    allowing view progression (but not finalization)
//! 4. **L-Notarization** (N-F votes): Once N-F votes are collected, an L-notarization is created,
//!    finalizing the block permanently
//! 5. **Nullification**: If a view times out or Byzantine behavior is detected (>2F conflicting
//!    messages), replicas create nullify messages. When 2F+1 nullifications are collected, the view
//!    is nullified and progression occurs using `SelectParent`
//! 6. **View Progression**: After M-notarization or nullification, replicas move to the next view
//!    and the new leader proposes
//!
//! ### Byzantine Fault Tolerance
//!
//! The protocol tolerates up to F Byzantine (arbitrarily malicious) replicas out of N total,
//! where N ≥ 5F+1. Byzantine behavior includes:
//! - Leaders proposing multiple conflicting blocks
//! - Replicas casting multiple votes
//! - Invalid signatures or messages
//! - Timeouts
//!
//! When >2F conflicting messages are detected, honest replicas nullify the view to make progress.
//!
//! ## Event-Driven Design
//!
//! The state machine doesn't directly implement consensus logic. Instead, it:
//! 1. Calls `ViewProgressManager` methods (e.g., `process_consensus_msg`, `tick`)
//! 2. Receives a [`ViewProgressEvent`] indicating what action to take
//! 3. Executes the action (propose, vote, broadcast, etc.)
//! 4. Returns to polling
//!
//! This separation keeps the state machine thin and delegates complex protocol logic to
//! `ViewProgressManager`, `ViewChain`, and `ViewContext`.
//!
//! ## Supported Events
//!
//! The state machine handles these [`ViewProgressEvent`] variants:
//!
//! - **`NoOp`**: No action needed
//! - **`Await`**: Waiting for more messages
//! - **`ShouldProposeBlock`**: Leader should propose a block for the current view
//! - **`ShouldVote`**: Replica should vote for a valid block
//! - **`ShouldMNotarize`**: Create and broadcast M-notarization (2F+1 votes collected)
//! - **`ShouldFinalize`**: Finalize a block (N-F votes collected)
//! - **`ShouldNullify`**: Create and broadcast nullify message (timeout or Byzantine)
//! - **`ShouldBroadcastNullification`**: Forward an aggregated nullification (2F+1 nullifies)
//! - **`ShouldVoteAndMNotarize`**: Vote crosses M-notarization threshold
//! - **`ShouldVoteAndFinalize`**: Vote crosses L-notarization threshold
//! - **`ProgressToNextView`**: Progress to next view after M-notarization
//! - **`ShouldVoteAndProgressToNextView`**: Vote, M-notarize, and progress atomically
//! - **`ProgressToNextViewOnNullification`**: Progress after nullification (2F+1 nullifies)
//! - **`ShouldUpdateView`**: Replica is behind and needs to catch up
//! - **`BroadcastConsensusMessage`**: Forward a message to all replicas
//!
//! ## Performance Characteristics
//!
//! - **Lock-Free Communication**: Uses [`rtrb`](https://docs.rs/rtrb) ring buffers for
//!   zero-allocation, wait-free message passing between threads
//! - **Non-Blocking Polls**: The event loop uses non-blocking `pop()` calls and spin hints to
//!   minimize latency while avoiding busy-waiting
//! - **Batching**: Processes all available messages before yielding, maximizing throughput
//! - **Retry Logic**: Broadcasts retry up to 10 times if the ring buffer is full, with 1ms backoff
//!   between attempts
//! - **Configurable Tick Interval**: Default 10ms tick interval balances timeout detection with CPU
//!   usage
//!
//! ## Security Considerations
//!
//! - **Secret Key Handling**: The `secret_key` field (`BlsSecretKey`) contains sensitive
//!   cryptographic material. Its inner field (`Fr` from `ark-ff`) implements `Zeroize`, ensuring
//!   memory is cleared on drop
//! - **Signature Operations**: All blocks, votes, and nullifications are signed with BLS
//!   signatures, ensuring authenticity and non-repudiation
//! - **Validation**: The `ViewProgressManager` validates all incoming messages (signatures, view
//!   numbers, block hashes) before processing
//!
//! ## Usage
//!
//! The state machine is typically created by [`ConsensusEngine`] and should not be
//! instantiated directly in production code. For testing, use [`ConsensusStateMachineBuilder`]:
//!
//! ```rust,ignore
//! use consensus::consensus_manager::state_machine::ConsensusStateMachineBuilder;
//! use std::{sync::{Arc, atomic::AtomicBool}, time::Duration};
//!
//! # fn example() -> anyhow::Result<()> {
//!     // Note: view_manager is created with ViewProgressManager::new() which takes
//!     // the PendingStateWriter for state persistence
//!     let mut state_machine = ConsensusStateMachineBuilder::<6, 1, 3>::new()
//!         .with_view_manager(view_manager)
//!         .with_secret_key(secret_key)
//!         .with_message_consumer(message_consumer)
//!         .with_broadcast_producer(broadcast_producer)
//!         .with_validated_block_consumer(validated_block_consumer)
//!         .with_tick_interval(Duration::from_millis(10))
//!     // Run the state machine (blocks until shutdown)
//!     state_machine.run()?;
//!     Ok(())
//! # }
//! ```
//! ## Thread Safety
//!
//! The state machine is **not** thread-safe and should only be accessed from the thread
//! that runs it. Communication with other threads occurs exclusively through the ring
//! buffers (`message_consumer`, `broadcast_producer`, `validated_block_consumer`), which
//! are lock-free and thread-safe.
//!
//! ## Relation to Minimmit Paper
//!
//! This implementation follows "Minimmit: A Minimal Byzantine Fault Tolerant Consensus
//! Protocol" with the following key correspondences:
//!
//! - **Algorithm 1 (Minimmit Processor)**: The `run()` event loop and `handle_event()` method
//!   implement the main processor logic
//! - **M-Notarization (2F+1)**: Handled in `create_and_broadcast_m_notarization()`
//! - **L-Notarization (N-F)**: Handled in `finalize_view()`
//! - **Nullification**: Handled in `nullify_view()`, with distinction between timeout and
//!   Byzantine-triggered nullifications
//! - **SelectParent**: Implemented in `ViewChain::select_parent()`, called by `ViewProgressManager`
//!   when progressing on nullification
//! - **Automatic Forwarding**: M-notarizations and nullifications are broadcast exactly once when
//!   first created, as per Algorithm 1 lines 2-3

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use rtrb::{Consumer, Producer};

use crate::{
    consensus::ConsensusMessage,
    consensus_manager::{events::ViewProgressEvent, view_manager::ViewProgressManager},
    crypto::aggregated::{BlsSecretKey, PeerId},
    mempool::{FinalizedNotification, ProposalRequest, ProposalResponse},
    state::{block::Block, notarizations::Vote, transaction::Transaction},
};

/// Maximum number of attempts to broadcast a consensus message, in case the ring buffer is full
const MAX_BROADCAST_ATTEMPTS: usize = 10;

/// Maximum number of transactions per block
const MAX_TRANSACTIONS_PER_BLOCK: usize = 1000;

/// Maximum block size in bytes
const MAX_BLOCK_SIZE_BYTES: usize = 1024 * 1024; // 1MB

/// Maximum time to wait for a proposal response in milliseconds
const FAST_PROPOSAL_RESPONSE_WAIT_MS: u64 = 10;

/// Maximum time to wait for a proposal response in milliseconds
const EXTENDED_PROPOSAL_RESPONSE_WAIT_MS: u64 = 50;

/// Consensus state machine that orchestrates the Minimmit protocol
pub struct ConsensusStateMachine<const N: usize, const F: usize, const M_SIZE: usize> {
    /// The view progress manager that drives consensus logic
    view_manager: ViewProgressManager<N, F, M_SIZE>,

    /// Buffer for messages received for future views
    pending_messages: BTreeMap<u64, Vec<ConsensusMessage<N, F, M_SIZE>>>,

    /// Secret key for signing messages
    secret_key: BlsSecretKey,

    /// Channel for receiving consensus messages from the network
    message_consumer: Consumer<ConsensusMessage<N, F, M_SIZE>>,

    /// Channel for broadcasting consensus messages to the network
    broadcast_producer: Producer<ConsensusMessage<N, F, M_SIZE>>,

    /// Channel for requesting block proposals from mempool
    proposal_req_producer: Producer<ProposalRequest>,

    /// Channel for receiving block proposals from mempool
    proposal_resp_consumer: Consumer<ProposalResponse>,

    /// Channel for notifying mempool of finalized transactions
    finalized_producer: Producer<FinalizedNotification>,

    /// Tick interval for checking timeouts and triggering periodic actions
    tick_interval: Duration,

    /// Signal to shutdown the state machine
    /// This is used to signal to the state machine that it should shutdown gracefully.
    shutdown_signal: Arc<AtomicBool>,

    /// Notify to wake up the P2P service when broadcast queue has data.
    broadcast_notify: Arc<tokio::sync::Notify>,

    /// Logger for logging events
    logger: slog::Logger,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> ConsensusStateMachine<N, F, M_SIZE> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        view_manager: ViewProgressManager<N, F, M_SIZE>,
        secret_key: BlsSecretKey,
        message_consumer: Consumer<ConsensusMessage<N, F, M_SIZE>>,
        broadcast_producer: Producer<ConsensusMessage<N, F, M_SIZE>>,
        proposal_req_producer: Producer<ProposalRequest>,
        proposal_resp_consumer: Consumer<ProposalResponse>,
        finalized_producer: Producer<FinalizedNotification>,
        tick_interval: Duration,
        shutdown_signal: Arc<AtomicBool>,
        broadcast_notify: Arc<tokio::sync::Notify>,
        logger: slog::Logger,
    ) -> Result<Self> {
        Ok(Self {
            view_manager,
            pending_messages: BTreeMap::new(),
            secret_key,
            message_consumer,
            broadcast_producer,
            proposal_req_producer,
            proposal_resp_consumer,
            finalized_producer,
            tick_interval,
            shutdown_signal,
            broadcast_notify,
            logger,
        })
    }

    /// Continuously processes messages from the network and transactions from the application.
    pub fn run(&mut self) -> Result<()> {
        slog::info!(
            self.logger,
            "Consensus state machine started for replica {}",
            self.view_manager.replica_id()
        );

        let mut last_tick = Instant::now();

        while !self.shutdown_signal.load(Ordering::Relaxed) {
            let mut did_work = false;

            // Process all available consensus messages from the network
            while let Ok(message) = self.message_consumer.pop() {
                did_work = true;
                if let Err(e) = self.handle_consensus_message(message) {
                    slog::warn!(self.logger, "Error handling consensus message: {}", e);
                }
            }

            // Periodic tick
            if last_tick.elapsed() >= self.tick_interval {
                did_work = true;
                match self.handle_tick() {
                    Ok(_) => {}
                    Err(e) => {
                        slog::warn!(self.logger, "Error handling tick: {}", e);
                    }
                }
                last_tick = Instant::now();
            }

            // Spin loop to prevent busy waiting
            if !did_work {
                std::thread::sleep(Duration::from_micros(500));
            }
        }

        slog::info!(
            self.logger,
            "Consensus state machine stopped for replica {}",
            self.view_manager.replica_id()
        );

        Ok(())
    }

    /// Stop the state machine gracefully
    pub fn stop(&mut self) -> Result<()> {
        self.shutdown_signal.store(true, Ordering::Relaxed);
        self.view_manager.shutdown()?;
        slog::info!(
            self.logger,
            "Consensus state machine stopped for replica {}",
            self.view_manager.replica_id()
        );
        Ok(())
    }

    /// Handles any incoming consensus messages
    fn handle_consensus_message(&mut self, message: ConsensusMessage<N, F, M_SIZE>) -> Result<()> {
        let event = self.view_manager.process_consensus_msg(message.clone())?;

        if let ViewProgressEvent::ShouldUpdateView { new_view, .. } = event {
            slog::info!(
                self.logger,
                "Buffering message for future view {}",
                new_view
            );
            self.pending_messages
                .entry(new_view)
                .or_default()
                .push(message);
            return Ok(());
        }

        self.handle_event(event)
    }

    /// Handles periodic tick
    pub fn handle_tick(&mut self) -> Result<()> {
        let event = self.view_manager.tick()?;
        self.handle_event(event)
    }

    /// Handles a view progress event by taking appropriate action
    fn handle_event(&mut self, event: ViewProgressEvent<N, F, M_SIZE>) -> Result<()> {
        match event {
            ViewProgressEvent::NoOp => {
                // Nothing to do
                Ok(())
            }
            ViewProgressEvent::Await => {
                // Waiting for messages
                Ok(())
            }
            ViewProgressEvent::ShouldProposeBlock {
                view,
                parent_block_hash,
            } => self.propose_block(view, parent_block_hash),
            ViewProgressEvent::ShouldVote { view, block_hash } => {
                self.vote_for_block(view, block_hash)
            }
            ViewProgressEvent::ShouldMNotarize {
                view,
                block_hash,
                should_forward_m_notarization,
            } => {
                // 1. Create and broadcast the M-notarization
                self.create_and_broadcast_m_notarization(
                    view,
                    block_hash,
                    should_forward_m_notarization,
                )?;

                // 2. Immediately process the M-notarization to advance state
                // Retrieve the M-notarization we just created
                if let Ok(m_not) = self.view_manager.get_m_notarization(view) {
                    // Process it synchronously to advance state
                    if let Ok(event) = self
                        .view_manager
                        .process_consensus_msg(ConsensusMessage::MNotarization(m_not))
                    {
                        self.handle_event(event)?;
                    }
                }

                Ok(())
            }
            ViewProgressEvent::ShouldFinalize { view, block_hash } => {
                self.finalize_view(view, block_hash)
            }
            ViewProgressEvent::ShouldNullify { view } => self.nullify_view(view, false),
            ViewProgressEvent::ShouldBroadcastNullification { view } => {
                self.broadcast_nullification(view)
            }
            ViewProgressEvent::ShouldVoteAndMNotarize {
                view,
                block_hash,
                should_forward_m_notarization,
            } => {
                // 1. Vote for the block and create and broadcast the M-notarization
                self.vote_for_block(view, block_hash)?;

                // 2. Create and broadcast the M-notarization
                self.create_and_broadcast_m_notarization(
                    view,
                    block_hash,
                    should_forward_m_notarization,
                )?;

                // 3. Immediately process the M-notarization to advance state
                if let Ok(m_not) = self.view_manager.get_m_notarization(view)
                    && let Ok(event) = self
                        .view_manager
                        .process_consensus_msg(ConsensusMessage::MNotarization(m_not))
                {
                    self.handle_event(event)?;
                }

                Ok(())
            }
            ViewProgressEvent::ShouldVoteAndFinalize { view, block_hash } => {
                self.vote_for_block(view, block_hash)?;
                self.finalize_view(view, block_hash)
            }
            ViewProgressEvent::ProgressToNextView {
                new_view,
                leader,
                notarized_block_hash,
                should_forward_m_notarization,
            } => {
                slog::info!(
                    self.logger,
                    "Progressed to view {} with M-notarization (block: {:?}, leader: {:?})",
                    new_view,
                    notarized_block_hash,
                    leader
                );
                self.create_and_broadcast_m_notarization(
                    new_view - 1, /* NOTE: The M-notarization is for the previous view (new_view
                                   * - 1) */
                    notarized_block_hash,
                    should_forward_m_notarization,
                )?;
                self.progress_to_next_view(new_view, leader, notarized_block_hash)
            }
            ViewProgressEvent::ShouldVoteAndProgressToNextView {
                old_view,
                block_hash,
                new_view,
                leader,
                should_forward_m_notarization,
            } => {
                self.vote_for_block(old_view, block_hash)?;
                self.create_and_broadcast_m_notarization(
                    old_view, // NOTE: The M-notarization is for the previous view (new_view - 1)
                    block_hash,
                    should_forward_m_notarization,
                )?;
                self.progress_to_next_view(new_view, leader, block_hash)?;
                Ok(())
            }
            ViewProgressEvent::ProgressToNextViewOnNullification {
                new_view,
                leader,
                parent_block_hash,
                should_broadcast_nullification,
            } => {
                if should_broadcast_nullification {
                    self.broadcast_nullification(new_view - 1)?; // NOTE: The nullification is for the previous view (new_view - 1)
                }
                self.progress_to_next_view(new_view, leader, parent_block_hash)?;
                Ok(())
            }
            ViewProgressEvent::ShouldUpdateView { new_view, leader } => {
                slog::warn!(
                    self.logger,
                    "Behind! Should update to view {} (leader: {:?})",
                    new_view,
                    leader
                );
                // TODO: In a full implementation, we'd request missing state here
                slog::info!(self.logger, "TODO: Request missing state here");
                Ok(())
            }
            ViewProgressEvent::ShouldNullifyView { view } => self.nullify_view(view, false),
            ViewProgressEvent::BroadcastConsensusMessage { message } => {
                self.broadcast_consensus_message(*message)
            }
            ViewProgressEvent::ShouldCascadeNullification {
                start_view,
                should_broadcast_nullification,
            } => {
                let current_view = self.view_manager.current_view_number();

                slog::warn!(
                    self.logger,
                    "Cascading nullification from view {} through {} (inclusive)",
                    start_view,
                    current_view
                );

                // 1. Broadcast the aggregated nullification for the view that triggered
                //    the cascade (if needed). This lets other nodes learn about the
                //    nullification and independently handle their own cascades.
                if should_broadcast_nullification
                    && let Err(e) = self.broadcast_nullification(start_view)
                {
                    slog::debug!(
                        self.logger,
                        "Failed to broadcast nullification for view {}: {}",
                        start_view,
                        e
                    );
                }

                // 2. Per paper Algorithm 1 Step 8: do NOT nullify intermediate views.
                //    Intermediate views between start_view and current_view were already
                //    processed through normal consensus flow (M-notarized or nullified).
                //    Marking them as nullified would corrupt their state and cause
                //    SelectParent to return inconsistent results across nodes, breaking
                //    chain integrity. Only the current view should be nullified.
                //    NOTE: We also do NOT remove pending diffs for intermediate views here.
                //    Those diffs are consistently present across all nodes and removing them
                //    would create state divergence (this node removes diffs that other nodes
                //    still have, causing InvalidNonce when this node proposes).

                // 3. Send a single Nullify for the current view (per paper Algorithm 1,
                //    Step 8). This broadcasts our vote AND marks it locally as nullified.
                if let Err(e) = self.nullify_view(current_view, true) {
                    slog::debug!(
                        self.logger,
                        "Failed to nullify current view {} during cascade: {}",
                        current_view,
                        e
                    );
                }

                // 4. Create a new view context and progress to it.
                //    Unlike normal nullification flow (which requires aggregated proof),
                //    cascade progression only requires local nullification of current view.
                let new_view = current_view + 1;
                let (leader, parent_hash) =
                    self.view_manager.progress_after_cascade(new_view)?;

                slog::info!(
                    self.logger,
                    "After cascade: progressing to new view {} with parent {:?}",
                    new_view,
                    parent_hash
                );

                // 5. Replay buffered messages and propose if leader
                self.progress_to_next_view(new_view, leader, parent_hash)?;

                Ok(())
            }
            ViewProgressEvent::ShouldNullifyRange { start_view } => {
                let current_view = self.view_manager.current_view_number();

                slog::warn!(
                    self.logger,
                    "Nullifying range from view {} through {} (inclusive)",
                    start_view,
                    current_view
                );

                // Per paper: do NOT nullify intermediate views between start_view and
                // current_view. They were already processed through normal consensus flow.
                // Marking them as nullified would corrupt their M-notarization state and
                // cause SelectParent inconsistencies across nodes.

                // Broadcast nullify for current view only
                if let Err(e) = self.nullify_view(current_view, true) {
                    slog::debug!(
                        self.logger,
                        "Failed to nullify current view {}: {}",
                        current_view,
                        e
                    );
                }

                Ok(())
            }
            ViewProgressEvent::ShouldRequestBlock { view, block_hash } => {
                slog::info!(
                    self.logger,
                    "Requesting missing block from peers";
                    "view" => view,
                );
                self.broadcast_consensus_message(
                    ConsensusMessage::BlockRecoveryRequest { view, block_hash },
                )
            }
        }
    }

    /// Proposes a block, as a leader, for the current view
    fn propose_block(&mut self, view: u64, parent_block_hash: [u8; blake3::OUT_LEN]) -> Result<()> {
        slog::debug!(
            self.logger,
            "Proposing block for view {view} with parent block hash {parent_block_hash:?}"
        );

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let proposal_request = ProposalRequest {
            view,
            max_txs: MAX_TRANSACTIONS_PER_BLOCK,
            max_bytes: MAX_BLOCK_SIZE_BYTES,
            parent_block_hash,
        };
        self.proposal_req_producer.push(proposal_request)?;

        // Wait for response (with timeout)
        let transactions = self.wait_for_proposal_response(view)?;

        // Create a dummy signature to construct the block structure.
        // The signature is NOT part of the block hash, so this doesn't affect the hash.
        // We sign a zero-byte array just to get a valid BlsSignature type.
        let dummy_signature = self.secret_key.sign(&[0u8; 32]);

        // Create the block. This internally calls compute_hash() on the contents.
        let mut block = Block::new(
            view,
            self.view_manager.replica_id(),
            parent_block_hash,
            transactions,
            timestamp,
            dummy_signature,
            false,
            view,
        );

        // Get the canonical hash from the block itself.
        // This is the exact hash that validators will re-compute.
        let block_hash = block.get_hash();

        // Sign the canonical hash with the leader's key.
        let leader_signature = self.secret_key.sign(&block_hash);

        // Update the block with the true leader signature.
        block.leader_signature = leader_signature.clone();

        // Broadcast the block proposal to the network layer (to be received by other replicas)
        self.broadcast_consensus_message(ConsensusMessage::BlockProposal(block.clone()))?;

        // Manually process the block proposal locally to avoid redundant vote broadcasting.
        // The leader's block proposal implicitly counts as a vote.
        let event = self
            .view_manager
            .process_consensus_msg(ConsensusMessage::BlockProposal(block))?;

        match event {
            ViewProgressEvent::ShouldVote { view, block_hash } => {
                // Leader implicitly votes via block proposal.
                // Update local state but DO NOT broadcast explicit Vote message.
                slog::debug!(
                    self.logger,
                    "Adding own vote for view {view} with block hash {block_hash:?}",
                );
                self.view_manager
                    .add_own_vote(view, block_hash, leader_signature)?;
            }
            ViewProgressEvent::ShouldVoteAndMNotarize {
                view,
                block_hash,
                should_forward_m_notarization,
            } => {
                slog::debug!(
                    self.logger,
                    "Adding own vote and creating M-notarization for view {view} with block hash {block_hash:?}",
                );
                self.view_manager
                    .add_own_vote(view, block_hash, leader_signature)?;
                self.create_and_broadcast_m_notarization(
                    view,
                    block_hash,
                    should_forward_m_notarization,
                )?;
            }
            ViewProgressEvent::ShouldVoteAndFinalize { view, block_hash } => {
                slog::debug!(
                    self.logger,
                    "Adding own vote and finalizing view {view} with block hash {block_hash:?}",
                );
                self.view_manager
                    .add_own_vote(view, block_hash, leader_signature)?;
                self.finalize_view(view, block_hash)?;
            }
            // Fallback for other events
            _ => {
                slog::warn!(
                    self.logger,
                    "Unexpected event after block proposal: {event:?}. Handling it anyway.",
                );
                self.handle_event(event)?;
            }
        }

        // Mark the block as proposed
        self.view_manager.mark_proposed(view)?;

        Ok(())
    }

    /// Vote for a block
    fn vote_for_block(&mut self, view: u64, block_hash: [u8; blake3::OUT_LEN]) -> Result<()> {
        slog::debug!(
            self.logger,
            "Voting for block for view {view} with block hash {block_hash:?}"
        );

        // Get leader for this view
        let leader_id = self.view_manager.leader_for_view(view)?;

        // Sign the vote
        let vote_signature = self.secret_key.sign(&block_hash);

        // Create a new vote
        let vote = Vote::new(
            view,
            block_hash,
            vote_signature.clone(),
            self.view_manager.replica_id(),
            leader_id,
        );

        let my_pk = self.secret_key.public_key();
        if !my_pk.verify(&block_hash, &vote_signature) {
            slog::error!(
                self.logger,
                "[DEBUG] CRITICAL: Local vote signing failed verification with own PK!"
            );
        }

        // Broadcast the vote to the network layer (to be received by other replicas)
        self.broadcast_consensus_message(ConsensusMessage::Vote(vote))?;

        // Mark the vote as cast
        // self.view_manager.mark_voted(view)?;

        // Add the vote to the view context
        self.view_manager
            .add_own_vote(view, block_hash, vote_signature)?;

        Ok(())
    }

    /// Create and broadcast an M-notarization for a block
    fn create_and_broadcast_m_notarization(
        &mut self,
        view: u64,
        block_hash: [u8; blake3::OUT_LEN],
        should_forward: bool,
    ) -> Result<()> {
        slog::debug!(
            self.logger,
            "Creating and broadcasting M-notarization for view {view} with block hash {block_hash:?}"
        );

        if should_forward {
            // Get the M-notarization for this view
            let m_notarization = self.view_manager.get_m_notarization(view)?;

            self.broadcast_consensus_message(ConsensusMessage::MNotarization(m_notarization))?;
        }

        Ok(())
    }

    /// Finalize a view with a L-notarization (n-f votes for the view block)
    fn finalize_view(&mut self, view: u64, block_hash: [u8; blake3::OUT_LEN]) -> Result<()> {
        slog::debug!(
            self.logger,
            "Finalizing view {view} with block hash {block_hash:?}"
        );

        // Defer finalization of the current view — removing it from non_finalized_views
        // would break invariants (current() would panic on the next tick).
        // The deferred finalization loop in progress_to_next_view will pick this up
        // once we advance to the next view.
        if view == self.view_manager.current_view_number() {
            return Ok(());
        }

        // Notify mempool to remove those transactions associated with the finalized block
        let finalized_txs = self.view_manager.get_block_tx_hashes(view)?;
        let notification = FinalizedNotification {
            view,
            tx_hashes: finalized_txs,
        };

        self.view_manager.finalize_view(view)?;

        // Best-effort notification (don't fail finalization if channel is full)
        best_effort_notification(&mut self.finalized_producer, notification, &self.logger)?;

        Ok(())
    }

    /// Nullify a view.
    ///
    /// # Arguments
    /// * `view` - The view number to nullify
    /// * `force` - If true, use cascade nullification (bypasses has_voted/evidence checks).
    ///   Used for ShouldCascadeNullification and ShouldNullifyRange events.
    ///   If false, use normal nullification logic (timeout or Byzantine based on evidence).
    fn nullify_view(&mut self, view: u64, force: bool) -> Result<()> {
        slog::debug!(self.logger, "Nullifying view {view} (force: {force})");

        // Get view context to create nullify message
        let view_ctx = self.view_manager.view_context_mut(view)?;

        if view_ctx.has_nullified {
            slog::info!(
                self.logger,
                "View {view} already nullified, ignoring request"
            );
            return Ok(());
        }

        let nullify = if force {
            // Cascade nullification: bypass has_voted check.
            // This is safe because cascade is for pending state consistency —
            // Lemma 5.3 guarantees that a nullified parent can never be L-notarized,
            // so descendant views' pending state must be cleaned up.
            view_ctx.create_nullify_for_cascade(&self.secret_key)?
        } else if view_ctx.has_voted {
            // After voting, we need conflicting evidence to nullify (Byzantine).
            let num_conflicting_votes = view_ctx.num_invalid_votes;
            let num_nullify_messages = view_ctx.nullify_messages.len();
            let combined_count = num_conflicting_votes + num_nullify_messages;

            if num_conflicting_votes > F || combined_count > 2 * F {
                // Byzantine behavior detected:
                // - Conflicting votes > F means equivocation (can't finalize)
                // - Combined evidence > 2F indicates Byzantine quorum
                view_ctx.create_nullify_for_byzantine(&self.secret_key)?
            } else {
                // Voted but no sufficient evidence — cannot nullify
                return Ok(());
            }
        } else {
            // Before voting: could be timeout OR Byzantine
            let num_conflicting_votes = view_ctx.num_invalid_votes;
            let num_nullify_messages = view_ctx.nullify_messages.len();
            let combined_count = num_conflicting_votes + num_nullify_messages;

            if num_conflicting_votes > F || combined_count > 2 * F {
                view_ctx.create_nullify_for_byzantine(&self.secret_key)?
            } else {
                // Timeout (no strong evidence of Byzantine behavior)
                view_ctx.create_nullify_for_timeout(&self.secret_key)?
            }
        };

        // Broadcast the nullify message to the network layer (to be received by other replicas)
        self.broadcast_consensus_message(ConsensusMessage::Nullify(nullify))?;

        // Mark the nullification as cast
        self.view_manager.mark_nullified(view)?;

        Ok(())
    }

    /// Broadcast a nullification for a view
    fn broadcast_nullification(&mut self, view: u64) -> Result<()> {
        slog::debug!(self.logger, "Broadcasting nullification for view {view}");

        let nullification = self.view_manager.get_nullification(view)?;

        self.broadcast_consensus_message(ConsensusMessage::Nullification(nullification))?;

        Ok(())
    }

    /// Progress to the next view after M-notarization, or nullification
    fn progress_to_next_view(
        &mut self,
        view: u64,
        leader: PeerId,
        notarized_block_hash: [u8; blake3::OUT_LEN],
    ) -> Result<()> {
        slog::debug!(
            self.logger,
            "Progressing to next view {view} with leader {leader} and notarized block hash {notarized_block_hash:?}"
        );

        // Try to finalize any pending views that now have enough votes.
        // This handles the case where votes accumulated during previous views
        // but finalization was deferred (e.g., missing block, missing parent M-notarization).
        while let Some((finalizable_view, block_hash)) =
            self.view_manager.oldest_finalizable_view()
        {
            slog::info!(
                self.logger,
                "Attempting deferred finalization for view {} (block: {:?})",
                finalizable_view,
                block_hash
            );
            if let Err(e) = self.finalize_view(finalizable_view, block_hash) {
                slog::warn!(
                    self.logger,
                    "Deferred finalization failed for view {}: {}",
                    finalizable_view,
                    e
                );
                break;
            }
            // If the view is still present after finalize_view returned Ok(()),
            // it means finalization was deferred (e.g., ancestor missing block).
            // Break to avoid an infinite loop — we'll retry on the next view progression.
            if self.view_manager.find_view_context(finalizable_view).is_some() {
                break;
            }
        }

        // Replay any buffered messages for this view (or previous ones)
        let pending_views: Vec<u64> = self
            .pending_messages
            .keys()
            .cloned()
            .filter(|&v| v <= view)
            .collect();

        for pending_view in pending_views {
            if let Some(messages) = self.pending_messages.remove(&pending_view) {
                slog::warn!(
                    self.logger,
                    "Replaying {} buffered messages for view {}",
                    messages.len(),
                    pending_view
                );
                for msg in messages {
                    if let Err(e) = self.handle_consensus_message(msg) {
                        slog::warn!(self.logger, "Failed to process buffered message: {}", e);
                    }
                }
            }
        }

        // Check if the current replica is the leader for the next view
        let replica_id = self.view_manager.replica_id();

        if replica_id == leader {
            // This replica is the leader for the next view, so it should propose a block
            let parent_block_hash = notarized_block_hash;
            self.propose_block(view, parent_block_hash)?;
        }

        Ok(())
    }

    /// Helper to broadcast a consensus message with retry on full buffer
    fn broadcast_consensus_message(
        &mut self,
        message: ConsensusMessage<N, F, M_SIZE>,
    ) -> Result<()> {
        for attempt in 0..MAX_BROADCAST_ATTEMPTS {
            match self.broadcast_producer.push(message.clone()) {
                Ok(_) => {
                    self.broadcast_notify.notify_one();
                    return Ok(());
                }
                Err(rtrb::PushError::Full(_)) => {
                    slog::warn!(
                        self.logger,
                        "Ring buffer is full, retrying, on attempt {} of {MAX_BROADCAST_ATTEMPTS}...",
                        attempt + 1,
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        Err(anyhow::anyhow!(
            "Failed to broadcast consensus message after {} attempts",
            MAX_BROADCAST_ATTEMPTS
        ))
    }

    /// Wait for a proposal response from the mempool
    fn wait_for_proposal_response(&mut self, view: u64) -> Result<Vec<Arc<Transaction>>> {
        let fast_deadline = Instant::now() + Duration::from_millis(FAST_PROPOSAL_RESPONSE_WAIT_MS);
        while Instant::now() < fast_deadline {
            if let Ok(response) = self.proposal_resp_consumer.pop()
                && response.view == view
            {
                return Ok(response.transactions);
            }
            std::hint::spin_loop();
        }

        // Extended wait (50ms) with sleep to reduce CPU usage
        let extended_deadline =
            Instant::now() + Duration::from_millis(EXTENDED_PROPOSAL_RESPONSE_WAIT_MS);
        while Instant::now() < extended_deadline {
            if let Ok(response) = self.proposal_resp_consumer.pop()
                && response.view == view
            {
                return Ok(response.transactions);
            }
            std::thread::sleep(Duration::from_micros(100));
        }

        // Timeout: propose empty block (liveness > latency)
        slog::debug!(
            self.logger,
            "Proposal response timeout for view {}, proposing empty block",
            view
        );

        Ok(vec![])
    }

    /// Gracefully shuts down the Minimmit consensus state machine
    pub fn shutdown(&mut self) -> Result<()> {
        self.shutdown_signal.store(true, Ordering::Relaxed);
        self.view_manager.shutdown()?;
        slog::info!(
            self.logger,
            "Consensus state machine stopped for replica {}",
            self.view_manager.replica_id()
        );
        Ok(())
    }
}

/// Best-effort notification to a producer
fn best_effort_notification<T>(
    producer: &mut Producer<T>,
    notification: T,
    logger: &slog::Logger,
) -> Result<()> {
    if let Err(rtrb::PushError::Full(_)) = producer.push(notification) {
        slog::warn!(logger, "Notification channel full, dropped notification");
    }
    Ok(())
}

/// Builder for creating a [`ConsensusStateMachine`] instance
pub struct ConsensusStateMachineBuilder<const N: usize, const F: usize, const M_SIZE: usize> {
    view_manager: Option<ViewProgressManager<N, F, M_SIZE>>,
    secret_key: Option<BlsSecretKey>,
    tick_interval: Option<Duration>,
    shutdown_signal: Option<Arc<AtomicBool>>,
    logger: Option<slog::Logger>,
    message_consumer: Option<Consumer<ConsensusMessage<N, F, M_SIZE>>>,
    broadcast_producer: Option<Producer<ConsensusMessage<N, F, M_SIZE>>>,
    broadcast_notify: Option<Arc<tokio::sync::Notify>>,
    proposal_req_producer: Option<Producer<ProposalRequest>>,
    proposal_resp_consumer: Option<Consumer<ProposalResponse>>,
    finalized_producer: Option<Producer<FinalizedNotification>>,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> Default
    for ConsensusStateMachineBuilder<N, F, M_SIZE>
{
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize, const F: usize, const M_SIZE: usize>
    ConsensusStateMachineBuilder<N, F, M_SIZE>
{
    pub fn new() -> Self {
        Self {
            view_manager: None,
            secret_key: None,
            tick_interval: None,
            shutdown_signal: None,
            logger: None,
            message_consumer: None,
            broadcast_producer: None,
            broadcast_notify: None,
            proposal_req_producer: None,
            proposal_resp_consumer: None,
            finalized_producer: None,
        }
    }

    pub fn with_view_manager(mut self, view_manager: ViewProgressManager<N, F, M_SIZE>) -> Self {
        self.view_manager = Some(view_manager);
        self
    }

    pub fn with_secret_key(mut self, secret_key: BlsSecretKey) -> Self {
        self.secret_key = Some(secret_key);
        self
    }

    pub fn with_tick_interval(mut self, interval: Duration) -> Self {
        self.tick_interval = Some(interval);
        self
    }

    pub fn with_shutdown_signal(mut self, shutdown_signal: Arc<AtomicBool>) -> Self {
        self.shutdown_signal = Some(shutdown_signal);
        self
    }

    pub fn with_logger(mut self, logger: slog::Logger) -> Self {
        self.logger = Some(logger);
        self
    }

    pub fn with_message_consumer(
        mut self,
        message_consumer: Consumer<ConsensusMessage<N, F, M_SIZE>>,
    ) -> Self {
        self.message_consumer = Some(message_consumer);
        self
    }

    pub fn with_broadcast_producer(
        mut self,
        broadcast_producer: Producer<ConsensusMessage<N, F, M_SIZE>>,
    ) -> Self {
        self.broadcast_producer = Some(broadcast_producer);
        self
    }

    pub fn with_broadcast_notify(mut self, broadcast_notify: Arc<tokio::sync::Notify>) -> Self {
        self.broadcast_notify = Some(broadcast_notify);
        self
    }

    pub fn with_proposal_req_producer(
        mut self,
        proposal_req_producer: Producer<ProposalRequest>,
    ) -> Self {
        self.proposal_req_producer = Some(proposal_req_producer);
        self
    }

    pub fn with_proposal_resp_consumer(
        mut self,
        proposal_resp_consumer: Consumer<ProposalResponse>,
    ) -> Self {
        self.proposal_resp_consumer = Some(proposal_resp_consumer);
        self
    }

    pub fn with_finalized_producer(
        mut self,
        finalized_producer: Producer<FinalizedNotification>,
    ) -> Self {
        self.finalized_producer = Some(finalized_producer);
        self
    }

    pub fn build(self) -> Result<ConsensusStateMachine<N, F, M_SIZE>> {
        ConsensusStateMachine::new(
            self.view_manager
                .ok_or_else(|| anyhow::anyhow!("ViewProgressManager not set"))?,
            self.secret_key
                .ok_or_else(|| anyhow::anyhow!("SecretKey not set"))?,
            self.message_consumer
                .ok_or_else(|| anyhow::anyhow!("Message consumer not set"))?,
            self.broadcast_producer
                .ok_or_else(|| anyhow::anyhow!("Broadcast producer not set"))?,
            self.proposal_req_producer
                .ok_or_else(|| anyhow::anyhow!("Proposal request producer not set"))?,
            self.proposal_resp_consumer
                .ok_or_else(|| anyhow::anyhow!("Proposal response consumer not set"))?,
            self.finalized_producer
                .ok_or_else(|| anyhow::anyhow!("Finalized producer not set"))?,
            self.tick_interval
                .ok_or_else(|| anyhow::anyhow!("Tick interval not set"))?,
            self.shutdown_signal
                .ok_or_else(|| anyhow::anyhow!("Shutdown signal not set"))?,
            self.broadcast_notify
                .ok_or_else(|| anyhow::anyhow!("Broadcast notify not set"))?,
            self.logger
                .ok_or_else(|| anyhow::anyhow!("Logger not set"))?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        consensus::ConsensusMessage,
        consensus_manager::{
            config::{ConsensusConfig, Network},
            leader_manager::{LeaderSelectionStrategy, RoundRobinLeaderManager},
            view_manager::ViewProgressManager,
        },
        crypto::{aggregated::BlsSecretKey, transaction_crypto::TxSecretKey},
        state::{address::Address, peer::PeerSet, transaction::Transaction},
        storage::store::ConsensusStore,
        validation::PendingStateWriter,
    };
    use ark_serialize::CanonicalSerialize;
    use rand::thread_rng;
    use rtrb::RingBuffer;
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };
    use tempfile::tempdir;

    // Test constants matching Minimmit assumptions: n >= 5f + 1
    const N: usize = 6; // 6 processors
    const F: usize = 1; // 1 Byzantine fault
    const M_SIZE: usize = 3; // M-notarization threshold: 2f + 1 = 3

    struct TestSetup {
        state_machines: Vec<ConsensusStateMachine<N, F, M_SIZE>>,
        peer_set: PeerSet,
        broadcast_consumers: Vec<Consumer<ConsensusMessage<N, F, M_SIZE>>>,
        shutdown_signals: Vec<Arc<AtomicBool>>,
        _peer_id_to_secret_key: HashMap<PeerId, BlsSecretKey>,
    }

    fn create_test_peer_setup(
        size: usize,
    ) -> (
        PeerSet,
        HashMap<crate::crypto::aggregated::PeerId, BlsSecretKey>,
    ) {
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

        (PeerSet::new(public_keys), peer_id_to_secret_key)
    }

    fn create_test_transaction(nonce: u64) -> Arc<Transaction> {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        Arc::new(Transaction::new_transfer(
            Address::from_public_key(&pk),
            Address::from_bytes([7u8; 32]),
            42,
            nonce,
            1_000,
            &sk,
        ))
    }

    fn create_test_config(n: usize, f: usize, peer_strs: Vec<String>) -> ConsensusConfig {
        ConsensusConfig {
            n,
            f,
            view_timeout: Duration::from_secs(10),
            leader_manager: LeaderSelectionStrategy::RoundRobin,
            network: Network::Local,
            peers: peer_strs,
            genesis_accounts: vec![],
        }
    }

    fn create_test_setup() -> TestSetup {
        const RING_BUFFER_SIZE: usize = 1000;

        let (peer_set, peer_id_to_secret_key) = create_test_peer_setup(N);
        let secret_keys: Vec<_> = peer_set
            .sorted_peer_ids
            .iter()
            .map(|id| peer_id_to_secret_key.get(id).unwrap().clone())
            .collect();

        let mut state_machines = Vec::with_capacity(N);
        let mut message_producers = Vec::with_capacity(N);
        let mut broadcast_consumers = Vec::with_capacity(N);
        let mut proposal_req_consumers = Vec::with_capacity(N);
        let mut proposal_resp_producers = Vec::with_capacity(N);
        let mut finalized_consumers = Vec::with_capacity(N);
        let mut shutdown_signals = Vec::with_capacity(N);
        let mut temp_dirs = Vec::with_capacity(N);

        for (i, sk) in secret_keys.iter().enumerate() {
            let replica_id = peer_set.sorted_peer_ids[i];

            // Create message channel (for incoming consensus messages)
            let (msg_prod, msg_cons) = RingBuffer::new(RING_BUFFER_SIZE);
            message_producers.push(msg_prod);

            // Create broadcast channel (for outgoing consensus messages)
            let (bc_prod, bc_cons) = RingBuffer::new(RING_BUFFER_SIZE);
            broadcast_consumers.push(bc_cons);

            // Create shutdown signal
            let shutdown = Arc::new(AtomicBool::new(false));
            shutdown_signals.push(shutdown.clone());

            // Create config
            let mut peer_strs = Vec::with_capacity(peer_set.sorted_peer_ids.len());
            for peer_id in &peer_set.sorted_peer_ids {
                let pk = peer_set.id_to_public_key.get(peer_id).unwrap();
                let mut buf = Vec::new();
                pk.0.serialize_compressed(&mut buf).unwrap();
                let peer_str = hex::encode(buf);
                peer_strs.push(peer_str);
            }
            let config = create_test_config(N, F, peer_strs);

            // Create logger
            let logger = slog::Logger::root(slog::Discard, slog::o!());

            // Create proposal request channel
            let (proposal_req_prod, proposal_req_cons) = RingBuffer::new(RING_BUFFER_SIZE);
            proposal_req_consumers.push(proposal_req_cons);

            // Create proposal response channel
            let (proposal_resp_prod, proposal_resp_cons) = RingBuffer::new(RING_BUFFER_SIZE);
            proposal_resp_producers.push(proposal_resp_prod);

            // Create finalized channel
            let (finalized_prod, finalized_cons) = RingBuffer::new(RING_BUFFER_SIZE);
            finalized_consumers.push(finalized_cons);

            // Create leader manager
            let leader_manager = Box::new(RoundRobinLeaderManager::new(
                N,
                peer_set.sorted_peer_ids.clone(),
            ));

            // Create persistence storage
            let temp_dir = tempdir().unwrap();
            let path = temp_dir.path().join(format!("state_machine_{}", i));
            let storage = Arc::new(ConsensusStore::open(&path).unwrap());
            let (persistence_writer, _persistence_reader) =
                PendingStateWriter::new(storage.clone(), 0);
            temp_dirs.push(temp_dir);

            // Create view manager
            let view_manager = ViewProgressManager::new(
                config,
                replica_id,
                leader_manager,
                persistence_writer,
                logger,
            )
            .unwrap();

            // Create logger
            let logger = slog::Logger::root(slog::Discard, slog::o!());

            // Create state machine
            let state_machine = ConsensusStateMachine::new(
                view_manager,
                sk.clone(),
                msg_cons,
                bc_prod,
                proposal_req_prod,
                proposal_resp_cons,
                finalized_prod,
                Duration::from_millis(100),
                shutdown,
                Arc::new(tokio::sync::Notify::new()),
                logger,
            )
            .unwrap();

            state_machines.push(state_machine);
        }

        TestSetup {
            state_machines,
            peer_set,
            broadcast_consumers,
            shutdown_signals,
            _peer_id_to_secret_key: peer_id_to_secret_key,
        }
    }

    #[test]
    fn test_minimmit_protocol_invariants() {
        // Test that key Minimmit invariants hold:
        // 1. M-notarization requires 2f+1 = 3 votes (with N=6, F=1)
        // 2. L-notarization requires n-f = 5 votes
        // 3. Nullification requires 2f+1 = 3 nullify messages
        assert_eq!(2 * F + 1, M_SIZE);
        assert_eq!(N - F, 5);
    }

    #[test]
    fn test_state_machine_creation() {
        // Test that we can create N state machines successfully
        let setup = create_test_setup();
        assert_eq!(setup.state_machines.len(), N);
        assert_eq!(setup.broadcast_consumers.len(), N);
        assert_eq!(setup.shutdown_signals.len(), N);
        assert_eq!(setup.peer_set.sorted_peer_ids.len(), N);
    }

    #[test]
    fn test_shutdown_stops_state_machine() {
        let mut setup = create_test_setup();
        let replica_idx = 0;

        // Shutdown should succeed
        let result = setup.state_machines[replica_idx].shutdown();
        assert!(result.is_ok());

        // Shutdown signal should be set
        assert!(setup.shutdown_signals[replica_idx].load(Ordering::Relaxed));
    }

    #[test]
    fn test_no_op_event_does_nothing() {
        let mut setup = create_test_setup();
        let replica_idx = 0;

        // NoOp event should succeed
        let event = ViewProgressEvent::NoOp;
        let result = setup.state_machines[replica_idx].handle_event(event);
        assert!(result.is_ok());

        // No messages should be broadcast
        let broadcast = setup.broadcast_consumers[replica_idx].pop();
        assert!(broadcast.is_err());
    }

    #[test]
    fn test_await_event_does_nothing() {
        let mut setup = create_test_setup();
        let replica_idx = 0;

        // Await event should succeed
        let event = ViewProgressEvent::Await;
        let result = setup.state_machines[replica_idx].handle_event(event);
        assert!(result.is_ok());

        // No messages should be broadcast
        let broadcast = setup.broadcast_consumers[replica_idx].pop();
        assert!(broadcast.is_err());
    }

    #[test]
    fn test_transaction_creation_has_valid_signature() {
        let tx = create_test_transaction(1);
        // Transaction should have valid signature
        assert!(tx.verify());

        // Different nonces should produce different transactions
        let tx2 = create_test_transaction(2);
        assert_ne!(tx.tx_hash, tx2.tx_hash);
    }

    #[test]
    fn test_peer_setup_creates_correct_number_of_peers() {
        let (peer_set, peer_id_to_secret_key) = create_test_peer_setup(N);

        assert_eq!(peer_set.sorted_peer_ids.len(), N);
        assert_eq!(peer_id_to_secret_key.len(), N);

        // All peer IDs should have corresponding secret keys
        for peer_id in &peer_set.sorted_peer_ids {
            assert!(peer_id_to_secret_key.contains_key(peer_id));
        }
    }

    #[test]
    fn test_consensus_config_creation() {
        let peers = vec!["peer1".to_string(), "peer2".to_string()];
        let config = create_test_config(N, F, peers.clone());

        assert_eq!(config.n, N);
        assert_eq!(config.f, F);
        assert_eq!(config.peers, peers);
        assert_eq!(config.view_timeout, Duration::from_secs(10));
        assert!(matches!(config.network, Network::Local));
        assert!(matches!(
            config.leader_manager,
            LeaderSelectionStrategy::RoundRobin
        ));
    }

    #[test]
    fn test_broadcast_channels_are_independent() {
        let mut setup = create_test_setup();

        // Each state machine should have its own broadcast channel
        // Verify we can access all of them
        for i in 0..N {
            assert!(setup.broadcast_consumers[i].pop().is_err()); // All empty initially
        }
    }

    use crate::validation::types::StateDiff;

    /// Creates a test block for a specific view and leader
    #[allow(dead_code)]
    fn create_block_for_view(
        view: u64,
        leader_id: PeerId,
        leader_sk: &BlsSecretKey,
        parent_hash: [u8; blake3::OUT_LEN],
    ) -> Block {
        let transactions = vec![create_test_transaction(1)];
        let temp_block = Block::new(
            view,
            leader_id,
            parent_hash,
            transactions.clone(),
            1234567890,
            leader_sk.sign(b"temp"),
            false,
            view,
        );
        let block_hash = temp_block.get_hash();
        Block::new(
            view,
            leader_id,
            parent_hash,
            transactions,
            1234567890,
            leader_sk.sign(&block_hash),
            false,
            view,
        )
    }

    #[test]
    fn test_synchronous_block_validation_on_tick() {
        // Scenario: State machine processes ticks correctly.
        // Block validation now happens synchronously in ViewProgressManager
        // when a BlockProposal is received via handle_block_proposal().
        let mut setup = create_test_setup();
        let replica_idx = 0;

        // Tick the state machine - should succeed even without messages
        let result = setup.state_machines[replica_idx].handle_tick();
        assert!(result.is_ok(), "Tick should succeed");

        // Initially no broadcasts since no blocks to vote on
        assert!(
            setup.broadcast_consumers[replica_idx].pop().is_err(),
            "Should have no broadcasts initially"
        );
    }

    #[test]
    fn test_handle_validated_block_with_empty_state_diff() {
        // Scenario: Block with empty StateDiff should still work
        let _setup = create_test_setup();

        // Empty StateDiff should not cause errors
        let empty_diff = StateDiff::new();
        assert!(empty_diff.updates.is_empty());
        assert!(empty_diff.created_accounts.is_empty());
        assert_eq!(empty_diff.total_fees, 0);
    }

    #[test]
    fn test_validated_block_struct_contains_state_diff() {
        // Verify ValidatedBlock struct has state_diff field
        use crate::validation::ValidatedBlock;

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());

        let mut state_diff = StateDiff::new();
        state_diff.add_created_account(addr, 5000);

        let block = Block::new(
            1,
            12345,
            [0u8; 32],
            vec![],
            1234567890,
            BlsSecretKey::generate(&mut thread_rng()).sign(b"test"),
            false,
            1,
        );

        let validated = ValidatedBlock::new(block.clone(), state_diff.clone());

        assert_eq!(validated.block.view(), 1);
        assert!(!validated.state_diff.created_accounts.is_empty());
    }
}
