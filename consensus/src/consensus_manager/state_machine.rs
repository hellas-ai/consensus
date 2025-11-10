use std::{
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
    state::{block::Block, notarizations::Vote, transaction::Transaction},
};

/// Maximum number of attempts to broadcast a consensus message, in case the ring buffer is full
const MAX_BROADCAST_ATTEMPTS: usize = 10;

/// Consensus state machine that orchestrates the Minimmit protocol
pub struct ConsensusStateMachine<const N: usize, const F: usize, const M_SIZE: usize> {
    /// The view progress manager that drives consensus logic
    view_manager: ViewProgressManager<N, F, M_SIZE>,

    /// Secret key for signing messages
    secret_key: BlsSecretKey,

    /// Channel for receiving consensus messages from the network
    message_consumer: Consumer<ConsensusMessage<N, F, M_SIZE>>,

    /// Channel for broadcasting consensus messages to the network
    broadcast_producer: Producer<ConsensusMessage<N, F, M_SIZE>>,

    /// Channel for receiving transactions to include in blocks
    transaction_consumer: Consumer<Transaction>,

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
        transaction_consumer: Consumer<Transaction>,
        tick_interval: Duration,
        shutdown_signal: Arc<AtomicBool>,
        logger: slog::Logger,
    ) -> Result<Self> {
        Ok(Self {
            view_manager,
            secret_key,
            message_consumer,
            broadcast_producer,
            transaction_consumer,
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
            // Process all available consensus messages from the network
            while let Ok(message) = self.message_consumer.pop() {
                if let Err(e) = self.handle_consensus_message(message) {
                    slog::error!(self.logger, "Error handling consensus message: {}", e);
                }
            }

            // Process all available transactions from the application
            while let Ok(transaction) = self.transaction_consumer.pop() {
                self.view_manager.add_transaction(transaction);
            }

            // Periodic tick
            if last_tick.elapsed() >= self.tick_interval {
                if let Err(e) = self.view_manager.tick() {
                    slog::error!(self.logger, "Error ticking view manager: {}", e);
                }
                last_tick = Instant::now();
            }

            // Spin loop to prevent busy waiting
            std::hint::spin_loop();
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
        let event = self.view_manager.process_consensus_msg(message)?;
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
            } => self.create_and_broadcast_m_notarization(
                view,
                block_hash,
                should_forward_m_notarization,
            ),
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
                self.vote_for_block(view, block_hash)?;
                self.create_and_broadcast_m_notarization(
                    view,
                    block_hash,
                    should_forward_m_notarization,
                )
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
                    new_view,
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
                    new_view,
                    block_hash,
                    should_forward_m_notarization,
                )?;
                self.progress_to_next_view(new_view, leader, block_hash)?;
                Ok(())
            }
            ViewProgressEvent::ProgressToNextViewOnNullification {
                new_view,
                leader,
                should_broadcast_nullification,
            } => {
                if should_broadcast_nullification {
                    // Already handled by the nullification broadcast logic
                }
                slog::info!(
                    self.logger,
                    "Progressed to view {} on nullification (leader: {:?})",
                    new_view,
                    leader
                );
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
        }
    }

    /// Proposes a block, as a leader, for the current view
    fn propose_block(&mut self, view: u64, parent_block_hash: [u8; blake3::OUT_LEN]) -> Result<()> {
        slog::debug!(
            self.logger,
            "Proposing block for view {view} with parent block hash {parent_block_hash:?}"
        );

        // Get pending transactions
        let transactions = self.view_manager.take_pending_transactions();

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let block_hash = compute_block_hash(parent_block_hash, &transactions, timestamp, view);
        let leader_signature = self.secret_key.sign(&block_hash);

        // Create a new block
        let block = Block::new(
            view,
            self.view_manager.replica_id(), /* It is the current leader replica that is
                                             * proposing the block */
            parent_block_hash,
            transactions,
            timestamp,
            leader_signature,
            false,
            view,
        );

        // Broadcast the block proposal to the network layer (to be received by other replicas)
        self.broadcast_consensus_message(ConsensusMessage::BlockProposal(block))?;

        // Mark the block as proposed
        self.view_manager.mark_proposed(view)?;

        // Mark the block as voted
        self.view_manager.mark_voted(view)?;

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

        // Broadcast the vote to the network layer (to be received by other replicas)
        self.broadcast_consensus_message(ConsensusMessage::Vote(vote))?;

        // Mark the vote as cast
        self.view_manager.mark_voted(view)?;

        // Add the vote to the view context
        self.view_manager.add_own_vote(view, vote_signature)?;

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

        let nullify = view_ctx.create_nullify_for_timeout(&self.secret_key)?;

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
    transaction_consumer: Option<Consumer<Transaction>>,
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
            transaction_consumer: None,
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

    pub fn with_transaction_consumer(
        mut self,
        transaction_consumer: Consumer<Transaction>,
    ) -> Self {
        self.transaction_consumer = Some(transaction_consumer);
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
            self.transaction_consumer
                .ok_or_else(|| anyhow::anyhow!("Transaction consumer not set"))?,
            self.tick_interval
                .ok_or_else(|| anyhow::anyhow!("Tick interval not set"))?,
            self.shutdown_signal
                .ok_or_else(|| anyhow::anyhow!("Shutdown signal not set"))?,
            self.logger
                .ok_or_else(|| anyhow::anyhow!("Logger not set"))?,
        )
    }
}

fn compute_block_hash(
    parent_block_hash: [u8; blake3::OUT_LEN],
    txs: &[Transaction],
    timestamp: u64,
    view: u64,
) -> [u8; blake3::OUT_LEN] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&parent_block_hash);
    hasher.update(
        &txs.iter()
            .enumerate()
            .map(|(i, t)| {
                let mut hasher = blake3::Hasher::new();
                hasher.update(&i.to_le_bytes());
                hasher.update(&t.tx_hash);
                hasher.finalize().into()
            })
            .collect::<Vec<[u8; blake3::OUT_LEN]>>()
            .concat(),
    );
    hasher.update(&timestamp.to_le_bytes());
    hasher.update(&view.to_le_bytes());
    hasher.finalize().into()
}
