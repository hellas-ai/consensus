//! Consensus Engine - High-Level Interface for the Minimmit BFT Consensus Protocol
//!
//! This module provides the [`ConsensusEngine`], which serves as the primary entry point
//! for running the Minimmit consensus protocol. It encapsulates the consensus state machine
//! in a dedicated thread and provides a simple, thread-safe API for external components
//! (such as P2P networking layers) to interact with the consensus system.
//!
//! ## Architecture
//!
//! The consensus engine follows a producer-consumer pattern using lock-free ring buffers
//! ([`rtrb`](https://docs.rs/rtrb)) for efficient inter-thread communication:
//!
//!
//! ┌───────────────────────────────────────────────────────────┐
//! │                    ConsensusEngine                        │
//! │  (Main Thread - API Interface)                            │
//! │                                                           │
//! │  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐     │
//! │  │   Message    │  │ Transaction  │  │  Broadcast   │     │
//! │  │   Producer   │  │   Producer   │  │   Consumer   │     │
//! │  └──────┬───────┘  └──────┬───────┘  └──────▲───────┘     │
//! └─────────┼──────────────────┼──────────────────┼───────────┘
//!           │                  │                  │
//!           │ Ring Buffers     │                  │
//!           │ (Lock-free)      │                  │
//!           ▼                  ▼                  │
//! ┌─────────┴──────────────────┴──────────────────┴─────────────┐
//! │              Consensus State Machine Thread                 │
//! │  (Dedicated Thread - Consensus Logic)                       │
//! │                                                             │
//! │  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐       │
//! │  │   Message    │  │ Transaction  │  │  Broadcast   │       │
//! │  │   Consumer   │  │   Consumer   │  │   Producer   │       │
//! │  └──────────────┘  └──────────────┘  └──────────────┘       │
//! │                                                             │
//! │  ┌────────────────────────────────────────────────┐         │
//! │  │      ViewProgressManager                       │         │
//! │  │  (Minimmit Protocol Implementation)            │         │
//! │  └────────────────────────────────────────────────┘         │
//! └─────────────────────────────────────────────────────────────┘
//! //!
//! ## Responsibilities
//!
//! The `ConsensusEngine` handles:
//!
//! - **Thread Management**: Spawns and manages a dedicated thread for consensus operations
//! - **Message Routing**: Routes incoming consensus messages (blocks, votes, M-notarizations,
//!   L-notarizations, nullifications) to the state machine
//! - **Transaction Submission**: Queues client transactions for inclusion in future blocks
//! - **Broadcast Distribution**: Receives consensus messages that need to be broadcast to other
//!   replicas and makes them available to the networking layer
//! - **Lifecycle Management**: Provides graceful shutdown with configurable timeout
//! - **Protocol Isolation**: Decouples the consensus logic from external concerns like network I/O
//!   and application logic
//!
//! ## Usage Example
//!
//!rust,ignore
//! use consensus::{
//!     consensus_manager::{
//!         consensus_engine::ConsensusEngine,
//!         config::ConsensusConfig,
//!     },
//!     crypto::aggregated::BlsSecretKey,
//!     storage::store::ConsensusStore,
//! };
//! use std::time::Duration;
//!
//! # fn example() -> anyhow::Result<()> {
//!     // Initialize consensus components
//!     let config = ConsensusConfig::default();
//!     let replica_id = 0;
//!     let secret_key = BlsSecretKey::generate(&mut rand::thread_rng());
//!     let storage = ConsensusStore::open("/path/to/db")?;
//!     let logger = slog::Logger::root(slog::Discard, slog::o!());
//!
//!     // Create and start the consensus engine
//!     let mut engine = ConsensusEngine::<6, 1, 3>::new(config, replica_id, secret_key, storage,
//! logger)?;     // Submit incoming consensus messages from the network
//!     engine.submit_consensus_message(incoming_message)?;
//!     // Submit client transactions
//!     engine.submit_transaction(transaction)?;
//!     // Check for messages to broadcast to other replicas
//!     for broadcast_msg in engine.recv_all_broadcasts() {
//!         network.broadcast(broadcast_msg)?;
//!     }
//!     // Later, gracefully shutdown
//!     engine.shutdown_and_wait(Duration::from_secs(10))?;
//!     Ok(())
//! # }
//! //!
//! ## Thread Safety
//!
//! The `ConsensusEngine` is designed for single-threaded ownership in the main application
//! thread, while the internal state machine runs in its own thread. Communication between
//! threads uses lock-free ring buffers, avoiding mutex contention and ensuring low latency.
//!
//! ## Performance Considerations
//!
//! - **Non-blocking Operations**: `submit_consensus_message`, `submit_transaction`, and
//!   `try_recv_broadcast` are all non-blocking and return immediately
//! - **Buffer Capacity**: Default buffer size is 10,000 messages. If your workload requires higher
//!   throughput, use [`with_capacity`](ConsensusEngine::with_capacity) to increase buffer sizes
//! - **Tick Interval**: The state machine polls at a default interval of 10ms. For lower-latency
//!   requirements, decrease this using `with_capacity`
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
    state::{peer::PeerSet, transaction::Transaction},
    storage::store::ConsensusStore,
};

/// Default capacity for internal ring buffers
const DEFAULT_BUFFER_CAPACITY: usize = 10_000;

/// Default tick interval for the consensus state machine
const DEFAULT_TICK_INTERVAL: Duration = Duration::from_millis(10);

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
    /// * `storage` - Persistent storage for consensus state
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A new `ConsensusEngine` instance that immediately starts running the consensus protocol
    /// in a dedicated thread.
    pub fn new(
        config: ConsensusConfig,
        replica_id: PeerId,
        secret_key: BlsSecretKey,
        storage: ConsensusStore,
        message_consumer: Consumer<ConsensusMessage<N, F, M_SIZE>>,
        broadcast_producer: Producer<ConsensusMessage<N, F, M_SIZE>>,
        transaction_consumer: Consumer<Transaction>,
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
        let view_manager = ViewProgressManager::new(config, replica_id, storage, leader_manager)
            .context("Failed to create ViewProgressManager")?;

        // Build consensus state machine
        let mut state_machine = ConsensusStateMachineBuilder::new()
            .with_view_manager(view_manager)
            .with_secret_key(secret_key)
            .with_message_consumer(message_consumer)
            .with_broadcast_producer(broadcast_producer)
            .with_transaction_consumer(transaction_consumer)
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
    use ark_serialize::CanonicalSerialize;
    use rand::thread_rng;
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
        let storage = ConsensusStore::open(temp_dir.path()).unwrap();

        // Create logger
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        // Create consensus engine
        let engine =
            ConsensusEngine::<N, F, M_SIZE>::new(config, replica_id, secret_key, storage, logger);

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
        let storage = ConsensusStore::open(temp_dir.path()).unwrap();
        let logger = slog::Logger::root(slog::Discard, slog::o!());

        let engine =
            ConsensusEngine::<N, F, M_SIZE>::new(config, replica_id, secret_key, storage, logger)
                .unwrap();

        assert!(engine.is_running());

        // Shutdown and wait
        let result = engine.shutdown_and_wait(Duration::from_secs(5));
        assert!(result.is_ok());
    }
}
