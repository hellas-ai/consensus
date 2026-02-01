//! ValidatorNode - High-level orchestrator for all node services.
//!
//! This module provides a unified interface for spawning and managing all
//! the services that make up a validator node:
//!
//! - Storage (ConsensusStore)
//! - Block Validation Service
//! - Mempool Service
//! - Consensus Engine
//! - P2P Service
//! - gRPC Server
//!
//! ## Runtime Architecture
//!
//! This node uses **two separate tokio runtimes**:
//!
//! 1. **P2P Runtime** (`commonware_runtime::tokio::Runner`)
//!    - Manages the P2P networking layer via commonware-p2p
//!    - Runs in a dedicated OS thread spawned by `spawn_p2p()`
//!    - The commonware `Runner::start()` pattern consumes the runner and blocks until completion,
//!      so it cannot be shared with other services
//!
//! 2. **gRPC Runtime** (standard `tokio::runtime::Runtime`)
//!    - Manages the tonic gRPC server for external API
//!    - Runs in a dedicated OS thread for isolation
//!
//! ### Why Separate Runtimes?
//!
//! The separate runtimes provide:
//! - **Workload isolation**: gRPC (bursty external requests) vs P2P (steady gossip)
//! - **No contention**: Services communicate via lock-free `ArrayQueue` and `Notify`
//! - **Independent shutdown**: Each runtime shuts down cleanly without blocking the other
//! - **Simplicity**: No complex lifetime management across runtime boundaries
//!
//! The cost is minimal: one extra OS thread and ~MB of tokio runtime overhead.
//!
//! ## Spawn Order
//!
//! Services are spawned in dependency order:
//!
//! ```text
//! 1. Storage         ─── Opens DB, creates ConsensusStore
//!        ↓
//! 2. Validation      ─── Creates PendingStateWriter/Reader  
//!        ↓
//! 3. Mempool         ─── Needs PendingStateReader
//!        ↓
//! 4. P2P             ─── Creates broadcast channels
//!        ↓
//! 5. Consensus       ─── Needs P2P's broadcast_notify
//!        ↓
//! 6. gRPC            ─── Needs tx queues, P2P handle
//! ```
//!
//! ## Shutdown Order
//!
//! The `run()` method waits for the consensus engine to exit (either via
//! ctrl-c signal or unexpected thread exit), then tears down remaining
//! services in dependency order:
//!
//! ```text
//! 1. Wait Consensus  ─── Runs until ctrl-c or crash
//! 2. Set shutdown flag─── Stops accepting new work
//! 3. Stop gRPC       ─── No new user transactions
//! 4. Signal P2P      ─── Stop accepting network messages
//! 5. Shutdown Mempool─── Safe (consensus is done)
//! 6. Wait P2P        ─── Finish pending broadcasts
//! ```
//!
//! Key insight: **Consensus depends on Mempool** for proposal building,
//! so Mempool must stay alive until Consensus finishes.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam::queue::ArrayQueue;
use rtrb::RingBuffer;
use slog::{Logger, o};

use tokio::sync::Notify;

use commonware_runtime::tokio::Runner as TokioRunner;
use consensus::consensus::ConsensusMessage;
use consensus::consensus_manager::config::ConsensusConfig;
use consensus::consensus_manager::consensus_engine::ConsensusEngine;
use consensus::mempool::MempoolService;
use consensus::state::transaction::Transaction;
use consensus::storage::store::ConsensusStore;
use consensus::validation::PendingStateWriter;
use grpc_client::config::RpcConfig;
use grpc_client::server::{RpcContext, RpcServer};
use p2p::ValidatorIdentity;
use p2p::config::P2PConfig;
use p2p::service::{P2PHandle, spawn as spawn_p2p};

use crate::config::NodeConfig;

/// Default buffer size for ring buffers and queues.
const BUFFER_SIZE: usize = 10_000;

/// Default tick interval for consensus state machine.
const DEFAULT_TICK_INTERVAL: Duration = Duration::from_millis(10);

/// High-level orchestrator for all node services.
///
/// `ValidatorNode` owns all the services and handles that make up a validator.
/// It provides a single entry point for spawning and shutting down the node.
///
/// # Example
///
/// ```ignore
/// let config = NodeConfig::from_path("config.toml")?;
/// let identity = ValidatorIdentity::generate();
/// let logger = create_logger();
///
/// let node = ValidatorNode::<6, 1, 3>::from_config(config, identity, logger)?;
/// node.run().await?;  // Blocks until ctrl-c or consensus exit
/// ```
pub struct ValidatorNode<const N: usize, const F: usize, const M_SIZE: usize> {
    /// P2P service handle for network operations.
    p2p_handle: P2PHandle,

    /// Consensus engine running the BFT protocol.
    consensus_engine: ConsensusEngine<N, F, M_SIZE>,

    /// Mempool service managing pending transactions.
    mempool_service: MempoolService,

    /// Persistent storage for blocks and state.
    storage: Arc<ConsensusStore>,

    /// Global shutdown flag shared across services.
    shutdown: Arc<AtomicBool>,

    /// gRPC server address.
    grpc_addr: SocketAddr,

    /// gRPC server shutdown signal.
    grpc_shutdown: Arc<Notify>,

    /// Node logger.
    logger: Logger,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> ValidatorNode<N, F, M_SIZE> {
    /// Spawns all node services from configuration.
    ///
    /// ## Spawn Order
    ///
    /// Services are spawned in dependency order:
    /// 1. Storage - Opens database
    /// 2. PendingState - Creates state readers/writers
    /// 3. Mempool - Transaction pool (needs pending state)
    /// 4. P2P - Network layer (creates tx broadcast queues)
    /// 5. gRPC - External API (needs P2P queues)
    /// 6. Consensus - BFT protocol (needs mempool channels + P2P notify)
    ///
    /// After spawning, call `wait_ready()` to wait for P2P bootstrap.
    ///
    /// # Arguments
    ///
    /// * `consensus_config` - Consensus protocol configuration
    /// * `p2p_config` - P2P networking configuration
    /// * `rpc_config` - gRPC server configuration
    /// * `identity` - Validator identity (BLS + Ed25519 keys)
    /// * `storage_path` - Path to the database directory
    /// * `logger` - Logger instance
    ///
    /// # Returns
    ///
    /// A `ValidatorNode` with all services spawned. Call `wait_ready()` before use.
    ///
    /// # Errors
    ///
    /// Returns an error if any service fails to start.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn<P: AsRef<Path>>(
        consensus_config: ConsensusConfig,
        p2p_config: P2PConfig,
        rpc_config: RpcConfig,
        identity: ValidatorIdentity,
        storage_path: P,
        logger: Logger,
    ) -> Result<Self> {
        let peer_id = identity.peer_id();
        let grpc_addr = rpc_config.listen_addr;

        slog::info!(
            logger,
            "Spawning ValidatorNode";
            "peer_id" => %peer_id,
            "storage_path" => %storage_path.as_ref().display(),
            "grpc_addr" => %grpc_addr,
        );

        // 1. Open Storage
        let storage = Arc::new(
            ConsensusStore::open(storage_path.as_ref())
                .context("Failed to open consensus store")?,
        );
        slog::debug!(logger, "Storage opened");

        // 2. Create PendingState
        let (persistence_writer, pending_state_reader) =
            PendingStateWriter::new(Arc::clone(&storage), 0);
        let grpc_pending_state_reader = pending_state_reader.clone();
        slog::debug!(logger, "PendingState created");

        // 3. Create shared shutdown flag
        let shutdown = Arc::new(AtomicBool::new(false));

        // 4. Create transaction queues
        // gRPC → Mempool: ArrayQueue (MPMC)
        let grpc_tx_queue = Arc::new(ArrayQueue::<Transaction>::new(BUFFER_SIZE));

        // P2P → Mempool: rtrb (SPSC)
        let (p2p_to_mempool_producer, p2p_to_mempool_consumer) =
            RingBuffer::<Transaction>::new(BUFFER_SIZE);

        // 5. Spawn Mempool Service
        let (mempool_service, mempool_channels) = MempoolService::spawn(
            Arc::clone(&grpc_tx_queue),
            p2p_to_mempool_consumer,
            pending_state_reader,
            Arc::clone(&shutdown),
            logger.new(o!("component" => "mempool")),
        );
        slog::debug!(logger, "Mempool service spawned");

        // 6. Create consensus message channels
        let (consensus_msg_producer, consensus_msg_consumer) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(BUFFER_SIZE);
        let (broadcast_producer, broadcast_consumer) =
            RingBuffer::<ConsensusMessage<N, F, M_SIZE>>::new(BUFFER_SIZE);

        // 7. Clone BLS key before moving identity
        let bls_secret_key = identity.bls_secret_key().clone();

        // 8. Spawn P2P Service (creates tx broadcast queues)
        let p2p_handle = spawn_p2p::<TokioRunner, N, F, M_SIZE>(
            TokioRunner::default(),
            p2p_config,
            identity,
            consensus_msg_producer,
            p2p_to_mempool_producer,
            broadcast_consumer,
            logger.new(o!("component" => "p2p")),
        );
        slog::debug!(logger, "P2P service spawned");

        // Get P2P's broadcast notify to share with consensus
        let broadcast_notify = Arc::clone(&p2p_handle.broadcast_notify);
        let p2p_ready = Arc::clone(&p2p_handle.is_ready);

        // 9. Create RPC Context and spawn gRPC server
        let rpc_context = RpcContext::new(
            Arc::clone(&storage),
            grpc_pending_state_reader,
            Some(mempool_channels.stats_reader.clone()), // mempool_stats
            None,                                        // peer_stats (TODO)
            None,                                        // block_events (TODO)
            None,                                        // consensus_events (TODO)
            None,                                        // tx_events (TODO)
            Arc::clone(&p2p_handle.tx_broadcast_queue),
            Arc::clone(&p2p_handle.tx_broadcast_notify),
            Arc::clone(&grpc_tx_queue),
            Arc::clone(&p2p_ready),
            logger.new(o!("component" => "grpc")),
        );

        // Create shutdown signal for gRPC server
        let grpc_shutdown = Arc::new(Notify::new());
        let grpc_shutdown_signal = Arc::clone(&grpc_shutdown);

        // Spawn gRPC server in a separate thread with its own Tokio runtime.
        // See module docs for why we use a separate runtime instead of sharing
        // the commonware runtime with P2P.
        let grpc_logger = logger.new(o!("component" => "grpc-server"));
        std::thread::Builder::new()
            .name("grpc-server".into())
            .spawn(move || {
                let rt = tokio::runtime::Runtime::new().expect("create tokio runtime for grpc");
                rt.block_on(async move {
                    let server = RpcServer::new(rpc_config, rpc_context);
                    let shutdown_future = async move {
                        grpc_shutdown_signal.notified().await;
                    };
                    if let Err(e) = server.serve_with_shutdown(shutdown_future).await {
                        slog::error!(grpc_logger, "gRPC server error"; "error" => %e);
                    }
                });
            })
            .context("Failed to spawn gRPC server thread")?;
        slog::debug!(logger, "gRPC server spawned"; "addr" => %grpc_addr);

        // 10. Spawn Consensus Engine
        let consensus_engine = ConsensusEngine::<N, F, M_SIZE>::new(
            consensus_config,
            peer_id,
            bls_secret_key,
            consensus_msg_consumer,
            broadcast_notify,
            broadcast_producer,
            mempool_channels.proposal_req_producer,
            mempool_channels.proposal_resp_consumer,
            mempool_channels.finalized_producer,
            persistence_writer,
            DEFAULT_TICK_INTERVAL,
            logger.new(o!("component" => "consensus")),
        )
        .context("Failed to create consensus engine")?;
        slog::debug!(logger, "Consensus engine spawned");

        slog::info!(
            logger,
            "ValidatorNode spawned - call wait_ready() to complete bootstrap";
            "grpc_addr" => %grpc_addr,
        );

        Ok(Self {
            p2p_handle,
            consensus_engine,
            mempool_service,
            storage,
            shutdown,
            grpc_addr,
            grpc_shutdown,
            logger,
        })
    }

    /// Spawns a validator node from a unified `NodeConfig`.
    ///
    /// This is the recommended way to create a `ValidatorNode`.
    /// The config can be loaded from TOML/YAML files or environment variables.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let config = NodeConfig::from_path("config.toml")?;
    /// let identity = ValidatorIdentity::generate();
    /// let logger = create_logger();
    ///
    /// let node = ValidatorNode::<6, 1, 3>::from_config(config, identity, logger)?;
    /// node.wait_ready().await;
    /// ```
    ///
    /// # Arguments
    ///
    /// * `config` - Complete node configuration
    /// * `identity` - Validator identity (BLS + Ed25519 keys)
    /// * `logger` - Logger instance
    ///
    /// # Returns
    ///
    /// A `ValidatorNode` with all services spawned. Call `wait_ready()` before use.
    ///
    /// # Errors
    ///
    /// Returns an error if any service fails to start.
    pub fn from_config(
        config: NodeConfig,
        identity: ValidatorIdentity,
        logger: Logger,
    ) -> Result<Self> {
        Self::spawn(
            config.consensus,
            config.p2p,
            config.rpc,
            identity,
            &config.storage.path,
            logger,
        )
    }

    /// Wait for the node to be fully ready (P2P bootstrap complete).
    ///
    /// This should be called after `spawn()` to ensure the P2P network
    /// is connected before the node starts participating in consensus.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let node = ValidatorNode::spawn(...)?;
    /// node.wait_ready().await;  // Wait for P2P bootstrap
    /// // Node is now ready for consensus
    /// ```
    pub async fn wait_ready(&self) {
        slog::info!(self.logger, "Waiting for P2P bootstrap...");
        self.p2p_handle.wait_ready().await;
        slog::info!(self.logger, "P2P bootstrap complete - node is ready");
    }

    /// Returns the P2P handle for network operations.
    pub fn p2p_handle(&self) -> &P2PHandle {
        &self.p2p_handle
    }

    /// Returns the gRPC server address.
    pub fn grpc_addr(&self) -> SocketAddr {
        self.grpc_addr
    }

    /// Returns the storage handle.
    pub fn storage(&self) -> Arc<ConsensusStore> {
        Arc::clone(&self.storage)
    }

    /// Checks if the node is healthy (all services running).
    pub fn is_healthy(&self) -> bool {
        !self.shutdown.load(Ordering::Acquire)
            && self.consensus_engine.is_running()
            && self.mempool_service.is_running()
            && self.p2p_handle.is_ready()
    }

    /// Runs the node until it stops (ctrl-c or a service crash), then shuts down.
    ///
    /// This waits for P2P bootstrap, then blocks until either a ctrl-c signal
    /// is received or the consensus engine exits unexpectedly. Then performs
    /// hierarchical shutdown of all services.
    pub async fn run(self) -> Result<()> {
        self.wait_ready().await;
        slog::info!(self.logger, "Node is ready and participating in consensus");

        let Self {
            p2p_handle,
            consensus_engine,
            mut mempool_service,
            shutdown,
            grpc_shutdown,
            logger,
            ..
        } = self;

        // Wait until ctrl-c or consensus thread exit
        let consensus_shutdown = consensus_engine.shutdown_signal();
        let consensus_future = consensus_engine.into_future();
        tokio::pin!(consensus_future);

        let consensus_result = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                slog::info!(logger, "Received ctrl-c");
                consensus_shutdown.store(true, Ordering::Relaxed);
                consensus_future.await
            }
            result = &mut consensus_future => result,
        };

        slog::info!(logger, "Beginning hierarchical shutdown");

        if let Err(ref e) = consensus_result {
            slog::error!(logger, "Consensus engine error"; "error" => %e);
        }
        slog::debug!(logger, "Consensus engine shutdown complete");

        // Set global shutdown flag
        shutdown.store(true, Ordering::Release);

        // Stop gRPC server
        grpc_shutdown.notify_one();
        slog::debug!(logger, "gRPC server shutdown signaled");

        // Signal P2P to stop
        p2p_handle.shutdown();
        slog::debug!(logger, "P2P service signaled");

        // Shutdown mempool (safe — consensus is done)
        mempool_service.shutdown();
        slog::debug!(logger, "Mempool service shutdown complete");

        // Wait for P2P thread to finish
        let _ = p2p_handle.join();
        slog::debug!(logger, "P2P service shutdown complete");

        slog::info!(logger, "ValidatorNode shutdown complete");
        consensus_result
    }
}

/// Builder for ValidatorNode with fluent configuration.
pub struct ValidatorNodeBuilder<const N: usize, const F: usize, const M_SIZE: usize> {
    consensus_config: Option<ConsensusConfig>,
    p2p_config: Option<P2PConfig>,
    rpc_config: Option<RpcConfig>,
    identity: Option<ValidatorIdentity>,
    storage_path: Option<String>,
    logger: Option<Logger>,
}

impl<const N: usize, const F: usize, const M_SIZE: usize> Default
    for ValidatorNodeBuilder<N, F, M_SIZE>
{
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize, const F: usize, const M_SIZE: usize> ValidatorNodeBuilder<N, F, M_SIZE> {
    /// Creates a new builder with no configuration.
    pub fn new() -> Self {
        Self {
            consensus_config: None,
            p2p_config: None,
            rpc_config: None,
            identity: None,
            storage_path: None,
            logger: None,
        }
    }

    /// Sets the consensus configuration.
    pub fn with_consensus_config(mut self, config: ConsensusConfig) -> Self {
        self.consensus_config = Some(config);
        self
    }

    /// Sets the P2P configuration.
    pub fn with_p2p_config(mut self, config: P2PConfig) -> Self {
        self.p2p_config = Some(config);
        self
    }

    /// Sets the gRPC server configuration.
    pub fn with_rpc_config(mut self, config: RpcConfig) -> Self {
        self.rpc_config = Some(config);
        self
    }

    /// Sets the validator identity.
    pub fn with_identity(mut self, identity: ValidatorIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Sets the storage path.
    pub fn with_storage_path(mut self, path: impl Into<String>) -> Self {
        self.storage_path = Some(path.into());
        self
    }

    /// Sets the logger.
    pub fn with_logger(mut self, logger: Logger) -> Self {
        self.logger = Some(logger);
        self
    }

    /// Builds and spawns the ValidatorNode.
    ///
    /// # Errors
    ///
    /// Returns an error if any required field is missing or if spawning fails.
    pub fn build(self) -> Result<ValidatorNode<N, F, M_SIZE>> {
        let consensus_config = self
            .consensus_config
            .ok_or_else(|| anyhow::anyhow!("consensus_config is required"))?;
        let p2p_config = self
            .p2p_config
            .ok_or_else(|| anyhow::anyhow!("p2p_config is required"))?;
        let rpc_config = self
            .rpc_config
            .ok_or_else(|| anyhow::anyhow!("rpc_config is required"))?;
        let identity = self
            .identity
            .ok_or_else(|| anyhow::anyhow!("identity is required"))?;
        let storage_path = self
            .storage_path
            .ok_or_else(|| anyhow::anyhow!("storage_path is required"))?;
        let logger = self
            .logger
            .unwrap_or_else(|| slog::Logger::root(slog::Discard, o!()));

        ValidatorNode::spawn(
            consensus_config,
            p2p_config,
            rpc_config,
            identity,
            &storage_path,
            logger,
        )
    }
}
