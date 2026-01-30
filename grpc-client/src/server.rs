//! gRPC server setup and context management.
//!
//! ## Runtime Architecture
//!
//! The gRPC server runs in its own dedicated tokio runtime, separate from the P2P
//! service which uses `commonware_runtime::tokio::Runner`. This isolation provides:
//!
//! - **Workload separation**: External API requests (bursty) don't compete with P2P gossip
//! - **Independent lifecycle**: gRPC can shut down without blocking P2P and vice versa
//! - **Clean shutdown**: The server accepts a shutdown signal for graceful termination
//!
//! Communication with P2P happens via lock-free `ArrayQueue` and `Notify` primitives,
//! which are `Send + Sync` and safe to share across runtime boundaries.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use consensus::mempool::MempoolStatsReader;
use consensus::state::transaction::Transaction;
use consensus::storage::store::ConsensusStore;
use consensus::validation::PendingStateReader;
use crossbeam::queue::ArrayQueue;
use p2p::PeerStatsReader;
use slog::Logger;
use tokio::sync::{Notify, broadcast};
use tonic::transport::Server;

use crate::config::RpcConfig;
use crate::proto::account_service_server::AccountServiceServer;
use crate::proto::admin_service_server::AdminServiceServer;
use crate::proto::block_service_server::BlockServiceServer;
use crate::proto::node_service_server::NodeServiceServer;
use crate::proto::subscription_service_server::SubscriptionServiceServer;
use crate::proto::transaction_service_server::TransactionServiceServer;
use crate::proto::{BlockEvent, ConsensusEvent, TransactionEvent};
use crate::services::{
    AccountServiceImpl, AdminServiceImpl, BlockServiceImpl, NodeServiceImpl,
    SubscriptionServiceImpl, TransactionServiceImpl,
};

/// Read-only context for services that only query state.
///
/// This context is `Send + Sync` and safe for use with tonic's async trait.
/// Used by: AccountService, BlockService, NodeService
#[derive(Clone)]
pub struct ReadOnlyContext {
    /// Storage for finalized state
    pub store: Arc<ConsensusStore>,
    /// Pending state reader for M-notarized state
    pub pending_state: PendingStateReader,
    /// Mempool stats reader (lock-free access to mempool statistics)
    pub mempool_stats: Option<MempoolStatsReader>,
    /// Peer stats reader (lock-free access to P2P peer information)
    pub peer_stats: Option<PeerStatsReader>,
    /// Block event sender for subscriptions
    pub block_events: Option<broadcast::Sender<BlockEvent>>,
    /// Consensus event sender for subscriptions
    pub consensus_events: Option<broadcast::Sender<ConsensusEvent>>,
    /// Transaction/mempool event sender for subscriptions
    pub tx_events: Option<broadcast::Sender<TransactionEvent>>,
    /// Logger
    pub logger: Logger,
}

/// Full context including transaction queues for services that submit transactions.
///
/// Uses lock-free ArrayQueues which are Sync, allowing concurrent access without Mutex.
/// Used by: TransactionService
pub struct RpcContext {
    /// Read-only context (can be cloned to services)
    pub read_only: ReadOnlyContext,
    /// Lock-free queue for broadcasting transactions via P2P network.
    pub p2p_tx_queue: Arc<ArrayQueue<Transaction>>,
    /// Notify to wake up P2P service when transaction is queued.
    pub p2p_tx_notify: Arc<Notify>,
    /// Lock-free queue for adding transactions to the local mempool.
    pub mempool_tx_queue: Arc<ArrayQueue<Transaction>>,
    /// P2P readiness flag (shared for health checks)
    pub p2p_ready: Arc<AtomicBool>,
}

impl RpcContext {
    /// Create a new RPC context.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<ConsensusStore>,
        pending_state: PendingStateReader,
        mempool_stats: Option<MempoolStatsReader>,
        peer_stats: Option<PeerStatsReader>,
        block_events: Option<broadcast::Sender<BlockEvent>>,
        consensus_events: Option<broadcast::Sender<ConsensusEvent>>,
        tx_events: Option<broadcast::Sender<TransactionEvent>>,
        p2p_tx_queue: Arc<ArrayQueue<Transaction>>,
        p2p_tx_notify: Arc<Notify>,
        mempool_tx_queue: Arc<ArrayQueue<Transaction>>,
        p2p_ready: Arc<AtomicBool>,
        logger: Logger,
    ) -> Self {
        Self {
            read_only: ReadOnlyContext {
                store,
                pending_state,
                mempool_stats,
                peer_stats,
                block_events,
                consensus_events,
                tx_events,
                logger,
            },
            p2p_tx_queue,
            p2p_tx_notify,
            mempool_tx_queue,
            p2p_ready,
        }
    }

    /// Get a clone of the read-only context.
    pub fn read_only_context(&self) -> ReadOnlyContext {
        self.read_only.clone()
    }
}

/// gRPC server instance.
pub struct RpcServer {
    config: RpcConfig,
    context: RpcContext,
}

impl RpcServer {
    /// Create a new RPC server with the given configuration and context.
    pub fn new(config: RpcConfig, context: RpcContext) -> Self {
        Self { config, context }
    }

    /// Start the gRPC server.
    ///
    /// This will block until the server is shut down.
    pub async fn serve(self) -> Result<(), tonic::transport::Error> {
        // Use a future that never completes for backward compatibility
        self.serve_with_shutdown(std::future::pending()).await
    }

    /// Start the gRPC server with a graceful shutdown signal.
    ///
    /// The server will stop accepting new connections when the `shutdown` future
    /// completes, then wait for existing connections to finish.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let shutdown_notify = Arc::new(Notify::new());
    /// let shutdown_clone = Arc::clone(&shutdown_notify);
    ///
    /// // In another task/thread:
    /// // shutdown_notify.notify_one();
    ///
    /// server.serve_with_shutdown(async move {
    ///     shutdown_clone.notified().await;
    /// }).await?;
    /// ```
    pub async fn serve_with_shutdown<F>(self, shutdown: F) -> Result<(), tonic::transport::Error>
    where
        F: Future<Output = ()> + Send,
    {
        let addr = self.config.listen_addr;

        slog::info!(
            self.context.read_only.logger,
            "Starting gRPC server";
            "address" => %addr,
        );

        // Create read-only service implementations
        let read_ctx = self.context.read_only_context();
        let account_service = AccountServiceImpl::new(read_ctx.clone());
        let block_service = BlockServiceImpl::new(read_ctx.clone());
        let tx_ctx = read_ctx.clone();
        let node_service = NodeServiceImpl::new(
            read_ctx,
            self.config.peer_id,
            self.config.network,
            self.config.total_validators,
            self.config.f,
            Arc::clone(&self.context.p2p_ready),
        );

        let transaction_service = TransactionServiceImpl::new(
            tx_ctx.clone(),
            Arc::clone(&self.context.p2p_tx_queue),
            Arc::clone(&self.context.p2p_tx_notify),
            Arc::clone(&self.context.mempool_tx_queue),
        );
        let subscription_service = SubscriptionServiceImpl::new(tx_ctx.clone());
        let admin_service = AdminServiceImpl::new(tx_ctx);

        let logger = self.context.read_only.logger.clone();

        // Build routes with implemented services
        let result = Server::builder()
            .add_service(AccountServiceServer::new(account_service))
            .add_service(AdminServiceServer::new(admin_service))
            .add_service(BlockServiceServer::new(block_service))
            .add_service(NodeServiceServer::new(node_service))
            .add_service(TransactionServiceServer::new(transaction_service))
            .add_service(SubscriptionServiceServer::new(subscription_service))
            .serve_with_shutdown(addr, shutdown)
            .await;

        slog::info!(logger, "gRPC server stopped");

        result
    }
}
