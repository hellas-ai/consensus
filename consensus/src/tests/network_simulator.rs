//! Network Simulator for Integration Tests
//!
//! This module provides a simulated local network that routes consensus messages
//! between replicas. It mimics the behavior of a real P2P network but runs entirely
//! in-process using lock-free ring buffers.
//!
//! ## Architecture
//!
//! The network routes messages according to:
//! - **BlockProposal** → Both Block Validator Thread and Consensus Thread
//!   - Block Validator: for async validation (produces StateDiff)
//!   - Consensus Thread: for immediate storage and quorum-based voting
//! - **Vote/MNot/Nullify** → Consensus Thread directly (via message_producer)
//!
//! ```text
//! ┌──────────────┐     ┌──────────────┐     ┌──────────────┐
//! │  Replica 1   │     │  Replica 2   │     │  Replica 3   │
//! │              │     │              │     │              │
//! │ Broadcast    │     │ Broadcast    │     │ Broadcast    │
//! │ Producer     │     │ Producer     │     │ Producer     │
//! └──────┬───────┘     └──────┬───────┘     └──────┬───────┘
//!        │                    │                    │
//!        └────────────────────┼────────────────────┘
//!                             ▼
//!                   ┌──────────────────┐
//!                   │  LocalNetwork    │
//!                   │  (Routing)       │
//!                   └──────────────────┘
//!                             │
//!                             ▼
//!                      BlockProposal
//!                    ┌────────┴────────┐
//!                    │                 │
//!                    ▼                 ▼
//!             ┌──────────────┐  ┌──────────────┐
//!             │ Block        │  │ Consensus    │
//!             │ Validator    │  │ Thread       │
//!             │ (async)      │  │ (immediate)  │
//!             └──────────────┘  └──────────────┘
//!                    │
//!                    ▼
//!             ValidatedBlock/ValidationFailure
//!                    │
//!                    ▼
//!             ┌──────────────┐
//!             │ Consensus    │
//!             │ Thread       │
//!             └──────────────┘
//! ```

use crate::{consensus::ConsensusMessage, crypto::aggregated::PeerId, state::block::Block};
use rtrb::{Consumer, Producer};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// Statistics for network performance monitoring
#[derive(Debug, Clone, Default)]
pub struct NetworkStats {
    /// Total number of messages routed
    pub messages_routed: Arc<AtomicU64>,

    /// Number of messages dropped (buffer full)
    pub messages_dropped: Arc<AtomicU64>,
}

impl NetworkStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn messages_routed(&self) -> u64 {
        self.messages_routed.load(Ordering::Relaxed)
    }

    pub fn messages_dropped(&self) -> u64 {
        self.messages_dropped.load(Ordering::Relaxed)
    }

    pub fn _reset(&self) {
        self.messages_routed.store(0, Ordering::Relaxed);
        self.messages_dropped.store(0, Ordering::Relaxed);
    }
}

/// Simulates a local network that routes messages between replicas
///
/// The network runs in a dedicated thread and continuously:
/// 1. Polls each replica's broadcast queue for outgoing messages
/// 2. Routes BlockProposal messages to the Block Validator (via block_producer)
/// 3. Routes other consensus messages (Vote/MNot/Nullify) directly to Consensus
/// 4. Tracks statistics about message routing
pub struct LocalNetwork<const N: usize, const F: usize, const M_SIZE: usize> {
    /// Map of replica_id -> message producer (for sending consensus messages TO that replica)
    /// Used for Vote, MNotarization, Nullify, Nullification messages
    pub(crate) message_producers:
        Arc<Mutex<HashMap<PeerId, Producer<ConsensusMessage<N, F, M_SIZE>>>>>,

    /// Map of replica_id -> block producer (for sending blocks TO that replica's validator)
    /// Used for BlockProposal messages - routes to Block Validator thread
    pub(crate) block_producers: Arc<Mutex<HashMap<PeerId, Producer<Block>>>>,

    /// Map of replica_id -> broadcast consumer (for receiving messages FROM that replica)
    broadcast_consumers: Arc<Mutex<HashMap<PeerId, Consumer<ConsensusMessage<N, F, M_SIZE>>>>>,

    /// Network routing thread handle
    routing_thread: Option<JoinHandle<()>>,

    /// Shutdown signal for network
    shutdown: Arc<AtomicBool>,

    /// Network performance statistics
    pub stats: NetworkStats,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> LocalNetwork<N, F, M_SIZE> {
    /// Creates a new local network simulator
    pub fn new() -> Self {
        Self {
            message_producers: Arc::new(Mutex::new(HashMap::new())),
            block_producers: Arc::new(Mutex::new(HashMap::new())),
            broadcast_consumers: Arc::new(Mutex::new(HashMap::new())),
            routing_thread: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            stats: NetworkStats::new(),
        }
    }

    /// Registers a replica's communication channels with the network
    ///
    /// # Arguments
    /// * `replica_id` - The peer ID of the replica
    /// * `message_producer` - Producer for sending consensus messages (Vote/MNot/Nullify) TO this replica
    /// * `block_producer` - Producer for sending blocks TO this replica's Block Validator
    /// * `broadcast_consumer` - Consumer for receiving messages FROM this replica
    pub fn register_replica(
        &mut self,
        replica_id: PeerId,
        message_producer: Producer<ConsensusMessage<N, F, M_SIZE>>,
        block_producer: Producer<Block>,
        broadcast_consumer: Consumer<ConsensusMessage<N, F, M_SIZE>>,
    ) {
        self.message_producers
            .lock()
            .unwrap()
            .insert(replica_id, message_producer);
        self.block_producers
            .lock()
            .unwrap()
            .insert(replica_id, block_producer);
        self.broadcast_consumers
            .lock()
            .unwrap()
            .insert(replica_id, broadcast_consumer);
    }

    /// Starts the network routing thread
    ///
    /// The routing thread continuously polls all replicas for outgoing messages
    /// and routes them according to ARCHITECTURE.md:
    /// - **BlockProposal** → Block Validator (via block_producer)
    /// - **Vote/MNot/Nullify** → Consensus (via message_producer)
    pub fn start(&mut self) {
        let message_producers = Arc::clone(&self.message_producers);
        let block_producers = Arc::clone(&self.block_producers);
        let consumers = Arc::clone(&self.broadcast_consumers);
        let shutdown = Arc::clone(&self.shutdown);
        let stats = self.stats.clone();

        let handle = thread::spawn(move || {
            // Get list of all replica IDs
            let consumer_ids: Vec<PeerId> = consumers.lock().unwrap().keys().copied().collect();

            while !shutdown.load(Ordering::Relaxed) {
                let mut did_work = false;

                // For each replica, check for outgoing messages
                for sender_id in &consumer_ids {
                    // Collect all messages from this replica
                    let messages: Vec<ConsensusMessage<N, F, M_SIZE>> = {
                        let mut consumers_lock = consumers.lock().unwrap();
                        if let Some(consumer) = consumers_lock.get_mut(sender_id) {
                            let mut msgs = Vec::new();
                            while let Ok(msg) = consumer.pop() {
                                msgs.push(msg);
                                did_work = true;
                            }
                            msgs
                        } else {
                            Vec::new()
                        }
                    };

                    // Route messages to all other replicas based on message type
                    if !messages.is_empty() {
                        let mut msg_producers_lock = message_producers.lock().unwrap();
                        let mut blk_producers_lock = block_producers.lock().unwrap();

                        for (receiver_id, msg_producer) in msg_producers_lock.iter_mut() {
                            // Don't send to self
                            if receiver_id == sender_id {
                                continue;
                            }

                            // Get the corresponding block producer for this receiver
                            let mut blk_producer = blk_producers_lock.get_mut(receiver_id);

                            for msg in &messages {
                                match msg {
                                    // BlockProposal → route to both:
                                    // 1. Block Validator (via block_producer) for async validation
                                    // 2. Consensus Thread (via message_producer) for immediate storage
                                    ConsensusMessage::BlockProposal(block) => {
                                        // Route to Block Validator for async validation
                                        if let Some(ref mut bp) = blk_producer {
                                            match bp.push(block.clone()) {
                                                Ok(_) => {
                                                    stats
                                                        .messages_routed
                                                        .fetch_add(1, Ordering::Relaxed);
                                                }
                                                Err(_) => {
                                                    stats
                                                        .messages_dropped
                                                        .fetch_add(1, Ordering::Relaxed);
                                                }
                                            }
                                        }
                                        // Also route to Consensus Thread for immediate storage
                                        // This allows voting based on quorum before validation completes
                                        match msg_producer.push(msg.clone()) {
                                            Ok(_) => {
                                                stats
                                                    .messages_routed
                                                    .fetch_add(1, Ordering::Relaxed);
                                            }
                                            Err(_) => {
                                                stats
                                                    .messages_dropped
                                                    .fetch_add(1, Ordering::Relaxed);
                                            }
                                        }
                                    }
                                    // All other messages → route directly to Consensus
                                    _ => match msg_producer.push(msg.clone()) {
                                        Ok(_) => {
                                            stats.messages_routed.fetch_add(1, Ordering::Relaxed);
                                        }
                                        Err(_) => {
                                            stats.messages_dropped.fetch_add(1, Ordering::Relaxed);
                                        }
                                    },
                                }
                            }
                        }
                    }
                }

                // Sleep briefly when idle to avoid busy-waiting
                if !did_work {
                    thread::sleep(Duration::from_micros(100));
                }
            }
        });

        self.routing_thread = Some(handle);
    }

    /// Checks if the network is currently running
    pub fn is_running(&self) -> bool {
        !self.shutdown.load(Ordering::Relaxed)
            && self
                .routing_thread
                .as_ref()
                .map(|h| !h.is_finished())
                .unwrap_or(false)
    }

    /// Shuts down the network routing thread
    pub fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.routing_thread.take() {
            let _ = handle.join();
        }
    }
}

impl<const N: usize, const F: usize, const M_SIZE: usize> Default for LocalNetwork<N, F, M_SIZE> {
    fn default() -> Self {
        Self::new()
    }
}
