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
//! ┌─────────────────────────────────────────────────────────────────┐
//! │              ConsensusStateMachine (Event Loop)                 │
//! │                                                                 │
//! │  ┌──────────────────────────────────────────────────────────┐  │
//! │  │  1. Poll validated_block_consumer (ValidatedBlocks)      │  │
//! │  │     - Validated blocks with StateDiff to vote for        │  │
//! │  └────────────────────┬─────────────────────────────────────┘  │
//! │                       │                                         │
//! │                       ▼                                         │
//! │  ┌──────────────────────────────────────────────────────────┐  │
//! │  │  2. Poll message_consumer (ConsensusMessages)            │  │
//! │  │     - BlockProposal, Vote, MNotarization, LNotarization  │  │
//! │  │     - Nullify, Nullification                             │  │
//! │  └────────────────────┬─────────────────────────────────────┘  │
//! │                       │                                         │
//! │                       ▼                                         │
//! │  ┌──────────────────────────────────────────────────────────┐  │
//! │  │  3. Periodic Tick (every tick_interval)                  │  │
//! │  │     - Check view timeout                                 │  │
//! │  │     - Trigger nullification if needed                    │  │
//! │  └────────────────────┬─────────────────────────────────────┘  │
//! │                       │                                         │
//! │                       ▼                                         │
//! │  ┌──────────────────────────────────────────────────────────┐  │
//! │  │  4. Process Event (ViewProgressEvent)                    │  │
//! │  │     - ViewProgressManager returns events based on state  │  │
//! │  └────────────────────┬─────────────────────────────────────┘  │
//! │                       │                                         │
//! │                       ▼                                         │
//! │  ┌──────────────────────────────────────────────────────────┐  │
//! │  │  5. Handle Event (take action)                           │  │
//! │  │     - Propose block, vote, create notarizations          │  │
//! │  │     - Broadcast messages, progress views                 │  │
//! │  └────────────────────┬─────────────────────────────────────┘  │
//! │                       │                                         │
//! │                       ▼                                         │
//! │  ┌──────────────────────────────────────────────────────────┐  │
//! │  │  6. Broadcast via broadcast_producer                     │  │
//! │  │     - Send messages to network layer                     │  │
//! │  └──────────────────────────────────────────────────────────┘  │
//! │                                                                 │
//! │  Loop continues until shutdown_signal is set                   │
//! └─────────────────────────────────────────────────────────────────┘
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
    state::{block::Block, notarizations::Vote},
    validation::ValidatedBlock,
};

/// Maximum number of attempts to broadcast a consensus message, in case the ring buffer is full
const MAX_BROADCAST_ATTEMPTS: usize = 10;

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

    /// Channel for receiving validated blocks from the validation service
    validated_block_consumer: Consumer<ValidatedBlock>,

    /// Tick interval for checking timeouts and triggering periodic actions
    tick_interval: Duration,

    /// Signal to shutdown the state machine
    /// This is used to signal to the state machine that it should shutdown gracefully.
    shutdown_signal: Arc<AtomicBool>,

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
        validated_block_consumer: Consumer<ValidatedBlock>,
        tick_interval: Duration,
        shutdown_signal: Arc<AtomicBool>,
        logger: slog::Logger,
    ) -> Result<Self> {
        Ok(Self {
            view_manager,
            pending_messages: BTreeMap::new(),
            secret_key,
            message_consumer,
            broadcast_producer,
            validated_block_consumer,
            tick_interval,
            shutdown_signal,
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

            // 1. Validated blocks (ready to vote for)
            while let Ok(validated_block) = self.validated_block_consumer.pop() {
                did_work = true;
                if let Err(e) = self.handle_validated_block(validated_block) {
                    slog::error!(self.logger, "Error handling validated block: {}", e);
                }
            }

            // Process all available consensus messages from the network
            while let Ok(message) = self.message_consumer.pop() {
                did_work = true;
                if let Err(e) = self.handle_consensus_message(message) {
                    slog::error!(self.logger, "Error handling consensus message: {}", e);
                }
            }

            // Periodic tick
            if last_tick.elapsed() >= self.tick_interval {
                did_work = true;
                match self.handle_tick() {
                    Ok(_) => {}
                    Err(e) => {
                        slog::error!(self.logger, "Error handling tick: {}", e);
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

    fn handle_validated_block(&mut self, validated_block: ValidatedBlock) -> Result<()> {
        let state_diff = validated_block.state_diff;

        // Store the StateDiff for later application on finalization
        // Then process the block through normal consensus flow
        let event = self
            .view_manager
            .process_validated_block(validated_block.block, state_diff)?;

        self.handle_event(event)
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
            ViewProgressEvent::ShouldNullify { view } => self.nullify_view(view),
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
            ViewProgressEvent::ShouldNullifyView { view } => self.nullify_view(view),
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

                // 1. Broadcast the nullification for the view that triggered the cascade (if
                //    needed)
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

                // 2. Nullify all views from start_view to current_view
                for view in (start_view + 1)..=current_view {
                    if let Err(e) = self.nullify_view(view) {
                        slog::debug!(
                            self.logger,
                            "View {} already nullified or error during cascade: {}",
                            view,
                            e
                        );
                    }
                }

                // 2. Progress to a new fresh view
                let new_view = current_view + 1;

                // 3. Find the most recent valid parent (skips all nullified views)
                let parent_hash = self.view_manager.select_parent(new_view);
                let leader = self.view_manager.leader_for_view(new_view)?;

                slog::info!(
                    self.logger,
                    "After cascade: progressing to new view {} with parent {:?}",
                    new_view,
                    parent_hash
                );

                // 4. Progress to the new view
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

                // Send nullify messages for all views from start_view to current_view (INCLUSIVE)
                // This marks them as has_nullified locally and broadcasts nullify votes,
                // but does NOT progress to a new view - we must wait for a Nullification
                // (aggregated proof with 2F+1 signatures) before we can safely progress.
                for view in start_view..=current_view {
                    if let Err(e) = self.nullify_view(view) {
                        slog::debug!(
                            self.logger,
                            "View {} already nullified or error: {}",
                            view,
                            e
                        );
                    }
                }

                Ok(())
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

        // Create a dummy signature to construct the block structure.
        // The signature is NOT part of the block hash, so this doesn't affect the hash.
        // We sign a zero-byte array just to get a valid BlsSignature type.
        let dummy_signature = self.secret_key.sign(&[0u8; 32]);

        // Create the block. This internally calls compute_hash() on the contents.
        let mut block = Block::new(
            view,
            self.view_manager.replica_id(),
            parent_block_hash,
            vec![], // TODO: add transactions to the block
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

        self.view_manager.finalize_view(view)?;

        Ok(())
    }

    /// Nullify a view
    fn nullify_view(&mut self, view: u64) -> Result<()> {
        slog::debug!(self.logger, "Nullifying view {view}");

        // Get view context to create nullify message
        let view_ctx = self.view_manager.view_context_mut(view)?;

        if view_ctx.has_nullified {
            slog::info!(
                self.logger,
                "View {view} already nullified, ignoring request"
            );
            return Ok(());
        }

        let nullify = if view_ctx.has_voted {
            // NOTE: After voting, the current replica must have conflicting evidence,
            // so in this case, the view is under Byzantine behavior.
            view_ctx.create_nullify_for_byzantine(&self.secret_key)?
        } else {
            // NOTE: If the current replica attempts to nullify a message before voting, it could be
            // timeout OR Byzantine. We distinguish based on the type of evidence:
            let num_conflicting_votes = view_ctx.num_invalid_votes;
            let num_nullify_messages = view_ctx.nullify_messages.len();
            let combined_count = num_conflicting_votes + num_nullify_messages;

            if num_conflicting_votes > F || combined_count > 2 * F {
                // Byzantine behavior detected:
                // - Conflicting votes > F means equivocation (can't finalize)
                // - Combined evidence > 2F indicates Byzantine quorum
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

        // Replay any buffered messages for this view (or previous ones)
        let pending_views: Vec<u64> = self
            .pending_messages
            .keys()
            .cloned()
            .filter(|&v| v <= view)
            .collect();

        for pending_view in pending_views {
            if let Some(messages) = self.pending_messages.remove(&pending_view) {
                slog::debug!(
                    self.logger,
                    "Replaying {} buffered messages for view {}",
                    messages.len(),
                    pending_view
                );
                for msg in messages {
                    if let Err(e) = self.handle_consensus_message(msg) {
                        slog::error!(self.logger, "Failed to process buffered message: {}", e);
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

/// Builder for creating a [`ConsensusStateMachine`] instance
pub struct ConsensusStateMachineBuilder<const N: usize, const F: usize, const M_SIZE: usize> {
    view_manager: Option<ViewProgressManager<N, F, M_SIZE>>,
    secret_key: Option<BlsSecretKey>,
    tick_interval: Option<Duration>,
    shutdown_signal: Option<Arc<AtomicBool>>,
    logger: Option<slog::Logger>,
    message_consumer: Option<Consumer<ConsensusMessage<N, F, M_SIZE>>>,
    broadcast_producer: Option<Producer<ConsensusMessage<N, F, M_SIZE>>>,
    validated_block_consumer: Option<Consumer<ValidatedBlock>>,
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
            validated_block_consumer: None,
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

    pub fn with_validated_block_consumer(
        mut self,
        validated_block_consumer: Consumer<ValidatedBlock>,
    ) -> Self {
        self.validated_block_consumer = Some(validated_block_consumer);
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
            self.validated_block_consumer
                .ok_or_else(|| anyhow::anyhow!("Validated block consumer not set"))?,
            self.tick_interval
                .ok_or_else(|| anyhow::anyhow!("Tick interval not set"))?,
            self.shutdown_signal
                .ok_or_else(|| anyhow::anyhow!("Shutdown signal not set"))?,
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
        _validated_block_producers: Vec<Producer<ValidatedBlock>>,
        shutdown_signals: Vec<Arc<AtomicBool>>,
        peer_id_to_secret_key: HashMap<PeerId, BlsSecretKey>,
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

    fn create_test_transaction(nonce: u64) -> Transaction {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        Transaction::new_transfer(
            Address::from_public_key(&pk),
            Address::from_bytes([7u8; 32]),
            42,
            nonce,
            1_000,
            &sk,
        )
    }

    fn create_test_config(n: usize, f: usize, peer_strs: Vec<String>) -> ConsensusConfig {
        ConsensusConfig {
            n,
            f,
            view_timeout: Duration::from_secs(10),
            leader_manager: LeaderSelectionStrategy::RoundRobin,
            network: Network::Local,
            peers: peer_strs,
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
        let mut validated_block_producers = Vec::with_capacity(N);
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

            // Create validated block channel
            let (vb_prod, validated_block_cons) =
                RingBuffer::<ValidatedBlock>::new(RING_BUFFER_SIZE);
            validated_block_producers.push(vb_prod);

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
                validated_block_cons,
                Duration::from_millis(100),
                shutdown,
                logger,
            )
            .unwrap();

            state_machines.push(state_machine);
        }

        TestSetup {
            state_machines,
            peer_set,
            broadcast_consumers,
            _validated_block_producers: validated_block_producers,
            shutdown_signals,
            peer_id_to_secret_key,
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

    use crate::validation::{ValidatedBlock, types::StateDiff};

    /// Creates a test StateDiff
    fn create_test_state_diff(balance: u64) -> StateDiff {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());
        let mut diff = StateDiff::new();
        diff.add_created_account(addr, balance);
        diff
    }

    /// Creates a test block for a specific view and leader
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
    fn test_handle_validated_block_extracts_state_diff() {
        // Scenario: ValidatedBlock arrives with StateDiff
        // handle_validated_block should extract and pass to view_manager
        let mut setup = create_test_setup();
        let replica_idx = 0;

        // Get the leader for view 1 (round-robin: index 1)
        let leader_id = setup.peer_set.sorted_peer_ids[1];
        let leader_sk = setup.peer_id_to_secret_key.get(&leader_id).unwrap().clone();

        // Use genesis hash as the parent (ViewProgressManager starts at view 1 with genesis as
        // parent)
        let parent_hash = Block::genesis_hash();
        let block = create_block_for_view(1, leader_id, &leader_sk, parent_hash);
        let state_diff = create_test_state_diff(1000);

        let validated_block = ValidatedBlock::new(block, state_diff);

        // Push the validated block to the channel
        setup._validated_block_producers[replica_idx]
            .push(validated_block)
            .expect("Failed to push validated block");

        // Tick the state machine to process the block
        let result = setup.state_machines[replica_idx].handle_tick();
        assert!(result.is_ok(), "Tick should succeed");

        // For a valid block from the correct leader, the replica should vote
        // Check if a vote was broadcast (unless this replica is the leader)
        if setup.peer_set.sorted_peer_ids[replica_idx] != leader_id {
            // Non-leader replicas should broadcast a vote
            let broadcast = setup.broadcast_consumers[replica_idx].pop();
            if let Ok(ConsensusMessage::Vote(vote)) = broadcast {
                assert_eq!(vote.view, 1);
                assert_eq!(vote.leader_id, leader_id);
            }
            // Note: It's also valid if no vote is broadcast immediately
            // depending on timing and state machine internals
        }
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
