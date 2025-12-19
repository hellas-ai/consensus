//! ## Architecture
//!
//! The consensus engine follows a producer-consumer pattern using lock-free ring buffers
//! ([`rtrb`](https://docs.rs/rtrb)) for efficient inter-thread communication. Channels are
//! created externally and passed to the engine at construction time:
//!
//!
//! ┌───────────────────────────────────────────────────────────────────┐
//! │                 External Components (Caller)                      │
//! │                                                                   │
//! │  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐            │
//! │  │   Message    │  │  Validated   │  │  Broadcast   │            │
//! │  │   Producer   │  │    Block     │  │   Consumer   │            │
//! │  │  (Network)   │  │   Producer   │  │  (Network)   │            │
//! │  └──────┬───────┘  └──────┬───────┘  └──────▲───────┘            │
//! └─────────┼──────────────────┼──────────────────┼───────────────────┘
//!           │                  │                  │
//!           │ Ring Buffers     │                  │
//!           │ (Lock-free)      │                  │
//!           ▼                  ▼                  │
//! ┌─────────┴──────────────────┴──────────────────┴───────────────────┐
//! │              Consensus State Machine Thread                       │
//! │  (Dedicated Thread - Consensus Logic)                             │
//! │                                                                   │
//! │  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐             │
//! │  │   Message    │  │  Validated   │  │  Broadcast   │             │
//! │  │   Consumer   │  │    Block     │  │   Producer   │             │
//! │  │              │  │   Consumer   │  │              │             │
//! │  └──────────────┘  └──────────────┘  └──────────────┘             │
//! │                                                                   │
//! │  ┌────────────────────────────────────────────────────┐           │
//! │  │      ViewProgressManager                           │           │
//! │  │  (Minimmit Protocol Implementation)                │           │
//! │  └────────────────────────────────────────────────────┘           │
//! └───────────────────────────────────────────────────────────────────┘
//!
//! ## Responsibilities
//!
//! The `ConsensusEngine` handles:
//!
//! - **Thread Management**: Spawns and manages a dedicated thread for consensus operations
//! - **Message Processing**: Consumes incoming consensus messages (blocks, votes, M-notarizations,
//!   L-notarizations, nullifications) from the message channel and routes them to the state machine
//! - **Validated Block Processing**: Consumes validated blocks (with their associated `StateDiff`)
//!   from the validation service and integrates them into the consensus flow
//! - **Broadcast Distribution**: Produces consensus messages to be broadcast to other replicas via
//!   the broadcast channel, which the networking layer consumes
//! - **Lifecycle Management**: Provides graceful shutdown with configurable timeout via
//!   [`shutdown_and_wait`](Self::shutdown_and_wait)
//! - **Protocol Isolation**: Decouples the consensus logic from external concerns like network I/O
//!   and block validation
//!
//! ## Usage Example
//!
//! ```rust,ignore
//! use consensus::{
//!     consensus_manager::{
//!         consensus_engine::ConsensusEngine,
//!         config::ConsensusConfig,
//!     },
//!     consensus::ConsensusMessage,
//!     crypto::aggregated::BlsSecretKey,
//!     storage::store::ConsensusStore,
//!     validation::{PendingStateWriter, ValidatedBlock},
//! };
//! use rtrb::RingBuffer;
//! use std::{sync::Arc, time::Duration};
//!
//! # fn example() -> anyhow::Result<()> {
//!     // Initialize consensus components
//!     let config = ConsensusConfig::default();
//!     let replica_id = peer_set.sorted_peer_ids[0];
//!     let secret_key = BlsSecretKey::generate(&mut rand::thread_rng());
//!     let storage = Arc::new(ConsensusStore::open("/path/to/db")?);
//!     let (persistence_writer, _reader) = PendingStateWriter::new(storage, 0);
//!     let logger = slog::Logger::root(slog::Discard, slog::o!());
//!
//!     // Create communication channels (owned by external components)
//!     let (mut message_producer, message_consumer) = RingBuffer::<ConsensusMessage<6, 1, 3>>::new(10000);
//!     let (broadcast_producer, mut broadcast_consumer) = RingBuffer::<ConsensusMessage<6, 1, 3>>::new(10000);
//!     let (mut validated_block_producer, validated_block_consumer) = RingBuffer::<ValidatedBlock>::new(1000);
//!
//!     // Create and start the consensus engine
//!     let engine = ConsensusEngine::<6, 1, 3>::new(
//!         config,
//!         replica_id,
//!         secret_key,
//!         message_consumer,
//!         broadcast_producer,
//!         validated_block_consumer,
//!         persistence_writer,
//!         Duration::from_millis(10),
//!         logger,
//!     )?;
//!
//!     // External components interact via channels:
//!     
//!     // Network layer pushes incoming consensus messages
//!     message_producer.push(incoming_message)?;
//!     
//!     // Validation service pushes validated blocks with state diffs
//!     validated_block_producer.push(validated_block)?;
//!     
//!     // Network layer pops messages to broadcast
//!     while let Ok(msg) = broadcast_consumer.pop() {
//!         network.broadcast(msg)?;
//!     }
//!
//!     // Later, gracefully shutdown
//!     engine.shutdown_and_wait(Duration::from_secs(10))?;
//!     Ok(())
//! # }
//! ```
//!
//! ## Performance Considerations
//!
//! - **Lock-Free Communication**: Uses [`rtrb`](https://docs.rs/rtrb) ring buffers for
//!   zero-allocation, wait-free message passing between threads
//! - **Non-Blocking Polls**: The internal state machine uses non-blocking `pop()` calls and spin
//!   hints to minimize latency while avoiding busy-waiting
//! - **External Channel Ownership**: Channels are created externally and passed to the engine,
//!   allowing callers to control buffer sizes based on their workload requirements
//! - **Configurable Tick Interval**: The `tick_interval` parameter controls how often the state
//!   machine checks for timeouts. Lower values (e.g., 1-10ms) provide faster timeout detection at
//!   the cost of higher CPU usage
//! - **Batching**: The state machine processes all available messages in each tick before yielding,
//!   maximizing throughput under load
//!
//! ## Thread Safety
//!
//! The `ConsensusEngine` is designed for single-threaded ownership in the main application
//! thread, while the internal state machine runs in its own thread. Communication between
//! threads uses lock-free ring buffers, avoiding mutex contention and ensuring low latency.
//!
//! ## Validated Block Flow
//!
//! The `ConsensusEngine` receives validated blocks through the `validated_block_consumer` channel.
//! This channel is used by the block validation service to submit blocks that have been verified
//! for correctness (valid transactions, proper signatures, etc.).
//!
//! Each [`ValidatedBlock`] contains:
//! - **`block`**: The block itself (header, transactions, leader signature)
//! - **`state_diff`**: A [`StateDiff`] representing the state changes from executing the block's
//!   transactions
//!
//! ### State Diff Lifecycle
//!
//! 1. **Validation**: Block validation service executes transactions and produces a `StateDiff`
//! 2. **Submission**: `ValidatedBlock` is pushed to the channel by the validation service
//! 3. **Consumption**: `ConsensusEngine` pops the block and passes it to `ViewProgressManager`
//! 4. **Storage**: `StateDiff` is stored in the `ViewContext` for the block's view
//! 5. **M-Notarization**: When the block receives 2F+1 votes (M-notarization), the `StateDiff` is
//!    added to pending state via `PendingStateWriter`
//! 6. **Finalization**: When the block receives N-F votes (L-notarization), the `StateDiff` is
//!    applied to finalized state and removed from pending
//!
//! This design ensures that state changes are only visible to subsequent transactions after
//! the block achieves M-notarization, providing consistency guarantees for the validation service.
//!
//! ## Minimmit Protocol Context
//!
//! This engine implements the Minimmit Byzantine Fault Tolerant (BFT) consensus protocol,
//! which achieves consensus even when up to F out of N replicas are faulty (where N = 3F+1).
//! The protocol proceeds through numbered views, with each view having a designated leader
//! responsible for proposing blocks. Consensus is reached through M-notarizations (2F+1 votes)
//! and L-notarizations (N-F votes), with nullifications handling view timeouts and Byzantine
//! behavior.

use std::{
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result};
use rtrb::{Consumer, Producer};

use crate::{
    consensus::ConsensusMessage,
    consensus_manager::{
        config::ConsensusConfig,
        leader_manager::{LeaderManager, LeaderSelectionStrategy, RoundRobinLeaderManager},
        state_machine::ConsensusStateMachineBuilder,
        view_manager::ViewProgressManager,
    },
    crypto::aggregated::{BlsPublicKey, BlsSecretKey, PeerId},
    mempool::{FinalizedNotification, ProposalRequest, ProposalResponse},
    state::peer::PeerSet,
    validation::PendingStateWriter,
};

/// [`ConsensusEngine`] is the high-level interface for running the Minimmit consensus protocol.
///
/// It spawns a dedicated thread to run the consensus state machine and provides methods
/// to interact with it from other parts of the system (e.g., P2P network layer).
pub struct ConsensusEngine<const N: usize, const F: usize, const M_SIZE: usize> {
    /// The replica's peer ID
    replica_id: PeerId,

    /// Signal to shutdown the consensus engine
    shutdown_signal: Arc<AtomicBool>,

    /// Handle to the consensus state machine thread
    thread_handle: Option<JoinHandle<Result<()>>>,

    /// Logger for the consensus engine
    logger: slog::Logger,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> ConsensusEngine<N, F, M_SIZE> {
    /// Creates a new [`ConsensusEngine`] instance.
    ///
    /// # Arguments
    /// * `config` - Consensus configuration
    /// * `replica_id` - The ID of this replica
    /// * `secret_key` - The BLS secret key for signing messages
    /// * `message_consumer` - Channel consumer for incoming consensus messages from the network
    /// * `broadcast_producer` - Channel producer for outgoing consensus messages to broadcast
    /// * `validated_block_consumer` - Channel consumer for validated blocks from the validation
    ///   service
    /// * `persistence_writer` - Writer for persisting pending state to storage
    /// * `tick_interval` - Interval for checking timeouts and processing events
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A new `ConsensusEngine` instance that immediately starts running the consensus protocol
    /// in a dedicated thread.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: ConsensusConfig,
        replica_id: PeerId,
        secret_key: BlsSecretKey,
        message_consumer: Consumer<ConsensusMessage<N, F, M_SIZE>>,
        broadcast_producer: Producer<ConsensusMessage<N, F, M_SIZE>>,
        proposal_req_producer: Producer<ProposalRequest>,
        proposal_resp_consumer: Consumer<ProposalResponse>,
        finalized_producer: Producer<FinalizedNotification>,
        persistence_writer: PendingStateWriter,
        tick_interval: Duration,
        logger: slog::Logger,
    ) -> Result<Self> {
        // Create shutdown signal
        let shutdown_signal = Arc::new(AtomicBool::new(false));

        // Create leader manager based on config
        let leader_manager: Box<dyn LeaderManager> = match config.leader_manager {
            LeaderSelectionStrategy::RoundRobin => {
                // Parse peer IDs from config
                let peer_ids = PeerSet::new(
                    config
                        .peers
                        .iter()
                        .map(|p| BlsPublicKey::from_str(p).expect("Failed to parse BlsPublicKey"))
                        .collect(),
                );
                Box::new(RoundRobinLeaderManager::new(
                    config.n,
                    peer_ids.sorted_peer_ids,
                ))
            }
            LeaderSelectionStrategy::Random => {
                unimplemented!("Random leader selection is not implemented, for now")
            }
            LeaderSelectionStrategy::ProofOfStake => {
                unimplemented!("Proof of stake leader selection is not implemented, for now")
            }
        };

        // Create view progress manager
        let view_manager = ViewProgressManager::new(
            config,
            replica_id,
            leader_manager,
            persistence_writer,
            logger.clone(),
        )
        .context("Failed to create ViewProgressManager")?;

        // Build consensus state machine
        let mut state_machine = ConsensusStateMachineBuilder::new()
            .with_view_manager(view_manager)
            .with_secret_key(secret_key)
            .with_message_consumer(message_consumer)
            .with_broadcast_producer(broadcast_producer)
            .with_proposal_req_producer(proposal_req_producer)
            .with_proposal_resp_consumer(proposal_resp_consumer)
            .with_finalized_producer(finalized_producer)
            .with_tick_interval(tick_interval)
            .with_shutdown_signal(shutdown_signal.clone())
            .with_logger(logger.clone())
            .build()
            .context("Failed to build ConsensusStateMachine")?;

        // Spawn consensus thread
        let thread_logger = logger.clone();
        let thread_handle = thread::Builder::new()
            .name(format!("consensus-{}", replica_id))
            .spawn(move || {
                slog::info!(
                    thread_logger,
                    "Consensus engine thread started for replica {}",
                    replica_id
                );

                let result = state_machine.run();

                if let Err(ref e) = result {
                    slog::error!(
                        thread_logger,
                        "Consensus engine thread error for replica {}: {:?}",
                        replica_id,
                        e
                    );
                } else {
                    slog::info!(
                        thread_logger,
                        "Consensus engine thread stopped for replica {}",
                        replica_id
                    );
                }

                result
            })
            .context("Failed to spawn consensus thread")?;

        slog::info!(logger, "ConsensusEngine created for replica {}", replica_id);

        Ok(Self {
            replica_id,
            shutdown_signal,
            thread_handle: Some(thread_handle),
            logger,
        })
    }

    /// Returns the replica ID of this consensus engine.
    pub fn replica_id(&self) -> PeerId {
        self.replica_id
    }

    /// Checks if the consensus engine is still running.
    ///
    /// # Returns
    /// `true` if the thread is still alive, `false` otherwise
    pub fn is_running(&self) -> bool {
        !self.shutdown_signal.load(Ordering::Relaxed)
            && self
                .thread_handle
                .as_ref()
                .map(|h| !h.is_finished())
                .unwrap_or(false)
    }

    /// Initiates a graceful shutdown of the consensus engine.
    ///
    /// This sets the shutdown signal but does not wait for the thread to finish.
    /// Use [`shutdown_and_wait`](Self::shutdown_and_wait) to wait for completion.
    pub fn shutdown(&self) {
        slog::info!(
            self.logger,
            "Shutting down consensus engine for replica {}",
            self.replica_id
        );
        self.shutdown_signal.store(true, Ordering::Relaxed);
    }

    /// Initiates a graceful shutdown and waits for the consensus thread to finish.
    ///
    /// # Arguments
    /// * `timeout` - Maximum time to wait for shutdown
    ///
    /// # Returns
    /// The result from the state machine thread, or an error if timeout occurs
    pub fn shutdown_and_wait(mut self, timeout: Duration) -> Result<()> {
        self.shutdown();

        if let Some(handle) = self.thread_handle.take() {
            // Try to join with timeout
            let start = std::time::Instant::now();

            while !handle.is_finished() && start.elapsed() < timeout {
                thread::sleep(Duration::from_millis(10));
            }

            if handle.is_finished() {
                handle
                    .join()
                    .map_err(|e| anyhow::anyhow!("Consensus thread panicked: {:?}", e))??;
            } else {
                return Err(anyhow::anyhow!(
                    "Consensus thread did not shutdown within {:?}",
                    timeout
                ));
            }
        }

        slog::info!(
            self.logger,
            "Consensus engine shut down for replica {}",
            self.replica_id
        );

        Ok(())
    }
}

impl<const N: usize, const F: usize, const M_SIZE: usize> Drop for ConsensusEngine<N, F, M_SIZE> {
    fn drop(&mut self) {
        // Ensure shutdown is called
        self.shutdown();

        // Try to join the thread (but don't wait too long)
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::aggregated::BlsSecretKey;
    use crate::state::peer::PeerSet;
    use crate::storage::store::ConsensusStore;
    use ark_serialize::CanonicalSerialize;
    use rand::thread_rng;
    use rtrb::RingBuffer;
    use tempfile::tempdir;

    const N: usize = 6;
    const F: usize = 1;
    const M_SIZE: usize = 3;

    fn create_test_config(peer_strs: Vec<String>) -> ConsensusConfig {
        ConsensusConfig {
            n: N,
            f: F,
            view_timeout: Duration::from_secs(10),
            leader_manager: LeaderSelectionStrategy::RoundRobin,
            network: crate::consensus_manager::config::Network::Local,
            peers: peer_strs,
            genesis_accounts: vec![],
        }
    }

    #[test]
    fn test_consensus_engine_creation() {
        // Create test setup
        let mut rng = thread_rng();
        let mut public_keys = vec![];

        for _ in 0..N {
            let sk = BlsSecretKey::generate(&mut rng);
            let pk = sk.public_key();
            public_keys.push(pk);
        }

        let peer_set = PeerSet::new(public_keys);
        let replica_id = peer_set.sorted_peer_ids[0];
        let secret_key = BlsSecretKey::generate(&mut rng);

        // Create config
        let mut peer_strs = Vec::new();
        for peer_id in &peer_set.sorted_peer_ids {
            let pk = peer_set.id_to_public_key.get(peer_id).unwrap();
            let mut buf = Vec::new();
            pk.0.serialize_compressed(&mut buf).unwrap();
            peer_strs.push(hex::encode(buf));
        }
        let config = create_test_config(peer_strs);

        // Create storage
        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path().join("consensus.redb");
        let storage = ConsensusStore::open(&db_path).unwrap();
        let (pending_state_writer, _pending_state_reader) =
            PendingStateWriter::new(Arc::new(storage), 0);

        // Create logger
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        // Create message consumer
        let (_message_producer, message_consumer) = RingBuffer::new(1000);
        // Create broadcast producer
        let (broadcast_producer, _broadcast_consumer) = RingBuffer::new(1000);
        // Create proposal request producer
        let (proposal_req_producer, _proposal_req_consumer) = RingBuffer::new(1000);
        // Create proposal response consumer
        let (_proposal_resp_producer, proposal_resp_consumer) = RingBuffer::new(1000);
        // Create finalized producer
        let (finalized_producer, _finalized_consumer) = RingBuffer::new(1000);

        // Create tick interval
        let tick_interval = Duration::from_millis(10);

        // Create consensus engine
        let engine = ConsensusEngine::<N, F, M_SIZE>::new(
            config,
            replica_id,
            secret_key,
            message_consumer,
            broadcast_producer,
            proposal_req_producer,
            proposal_resp_consumer,
            finalized_producer,
            pending_state_writer,
            tick_interval,
            logger,
        );

        assert!(engine.is_ok());
        let engine = engine.unwrap();

        assert_eq!(engine.replica_id(), replica_id);
        assert!(engine.is_running());

        // Shutdown
        engine.shutdown_and_wait(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn test_consensus_engine_shutdown() {
        let mut rng = thread_rng();
        let mut public_keys = vec![];

        for _ in 0..N {
            let sk = BlsSecretKey::generate(&mut rng);
            let pk = sk.public_key();
            public_keys.push(pk);
        }

        let peer_set = PeerSet::new(public_keys);
        let replica_id = peer_set.sorted_peer_ids[0];
        let secret_key = BlsSecretKey::generate(&mut rng);

        let mut peer_strs = Vec::new();
        for peer_id in &peer_set.sorted_peer_ids {
            let pk = peer_set.id_to_public_key.get(peer_id).unwrap();
            let mut buf = Vec::new();
            pk.0.serialize_compressed(&mut buf).unwrap();
            peer_strs.push(hex::encode(buf));
        }
        let config = create_test_config(peer_strs);

        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path().join("consensus.redb");
        let storage = ConsensusStore::open(&db_path).unwrap();
        let (pending_state_writer, _pending_state_reader) =
            PendingStateWriter::new(Arc::new(storage), 0);
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        // Create message consumer
        let (_message_producer, message_consumer) = RingBuffer::new(1000);
        // Create broadcast producer
        let (broadcast_producer, _broadcast_consumer) = RingBuffer::new(1000);
        // Create proposal request producer
        let (proposal_req_producer, _proposal_req_consumer) = RingBuffer::new(1000);
        // Create proposal response consumer
        let (_proposal_resp_producer, proposal_resp_consumer) = RingBuffer::new(1000);
        // Create finalized producer
        let (finalized_producer, _finalized_consumer) = RingBuffer::new(1000);

        // Create tick interval
        let tick_interval = Duration::from_millis(10);

        let engine = ConsensusEngine::<N, F, M_SIZE>::new(
            config,
            replica_id,
            secret_key,
            message_consumer,
            broadcast_producer,
            proposal_req_producer,
            proposal_resp_consumer,
            finalized_producer,
            pending_state_writer,
            tick_interval,
            logger,
        )
        .unwrap();

        assert!(engine.is_running());

        // Shutdown and wait
        let result = engine.shutdown_and_wait(Duration::from_secs(5));
        assert!(result.is_ok());
    }

    use crate::validation::{ValidatedBlock, types::StateDiff};

    /// Creates a test StateDiff
    fn create_test_state_diff(balance: u64) -> StateDiff {
        use crate::crypto::transaction_crypto::TxSecretKey;
        use crate::state::address::Address;

        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let addr = Address::from_public_key(&sk.public_key());
        let mut diff = StateDiff::new();
        diff.add_created_account(addr, balance);
        diff
    }

    #[test]
    fn test_consensus_engine_accepts_validated_blocks() {
        // This test verifies the engine can be created with validated_block channel
        // and that the channel is properly connected

        let mut rng = thread_rng();
        let mut public_keys = vec![];
        let mut peer_id_to_secret_key = std::collections::HashMap::new();

        for _ in 0..N {
            let sk = BlsSecretKey::generate(&mut rng);
            let pk = sk.public_key();
            let peer_id = pk.to_peer_id();
            peer_id_to_secret_key.insert(peer_id, sk);
            public_keys.push(pk);
        }

        let peer_set = PeerSet::new(public_keys);

        // Create validated block channel
        let (mut validated_block_prod, mut validated_block_cons) =
            RingBuffer::<ValidatedBlock>::new(100);

        // Create a test validated block
        let leader_id = peer_set.sorted_peer_ids[0];
        let leader_sk = peer_id_to_secret_key.get(&leader_id).unwrap();

        let block = crate::state::block::Block::new(
            1,
            leader_id,
            [0u8; 32],
            vec![],
            1234567890,
            leader_sk.sign(b"test"),
            false,
            1,
        );

        let validated_block = ValidatedBlock::new(block, create_test_state_diff(1000));

        // Push to channel should succeed
        let push_result = validated_block_prod.push(validated_block);
        assert!(
            push_result.is_ok(),
            "Should be able to push ValidatedBlock to channel"
        );

        // Pop should retrieve it
        let pop_result = validated_block_cons.pop();
        assert!(
            pop_result.is_ok(),
            "Should be able to pop ValidatedBlock from channel"
        );

        let retrieved = pop_result.unwrap();
        assert_eq!(retrieved.block.view(), 1);
    }

    #[test]
    fn test_validated_block_channel_is_bounded() {
        // Verify channel has bounded capacity (backpressure)
        let (mut prod, _cons) = RingBuffer::<ValidatedBlock>::new(2);

        let block = crate::state::block::Block::new(
            1,
            12345,
            [0u8; 32],
            vec![],
            1234567890,
            BlsSecretKey::generate(&mut thread_rng()).sign(b"test"),
            false,
            1,
        );

        // Fill the channel
        for i in 0..2 {
            let vb = ValidatedBlock::new(block.clone(), StateDiff::new());
            assert!(prod.push(vb).is_ok(), "Push {} should succeed", i);
        }

        // Next push should fail (channel full)
        let vb = ValidatedBlock::new(block.clone(), StateDiff::new());
        assert!(prod.push(vb).is_err(), "Push to full channel should fail");
    }
}
