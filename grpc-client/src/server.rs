//! gRPC server setup and configuration.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use consensus::mempool::MempoolStatsReader;
use consensus::storage::store::ConsensusStore;
use consensus::validation::PendingStateReader;
use p2p::PeerStatsReader;
use p2p::service::P2PHandle;
use slog::Logger;
use tonic::service::Routes;
use tonic::transport::Server;

use crate::proto::account_service_server::AccountServiceServer;
use crate::proto::block_service_server::BlockServiceServer;
use crate::proto::node_service_server::NodeServiceServer;
use crate::services::{AccountServiceImpl, BlockServiceImpl, NodeServiceImpl};

/// Configuration for the RPC server.
#[derive(Debug, Clone)]
pub struct RpcConfig {
    /// Address to listen on (e.g., "0.0.0.0:50051")
    pub listen_addr: SocketAddr,
    /// Maximum concurrent streams per connection
    pub max_concurrent_streams: u32,
    /// Request timeout in seconds
    pub request_timeout_secs: u64,
    /// This node's peer ID
    pub peer_id: u64,
    /// Network name (e.g., "mainnet", "testnet", "local")
    pub network: String,
    /// Total validators in network
    pub total_validators: u32,
    /// Fault tolerance parameter F
    pub f: u32,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:50051".parse().unwrap(),
            max_concurrent_streams: 100,
            request_timeout_secs: 30,
            peer_id: 0,
            network: "local".to_string(),
            total_validators: 4,
            f: 1,
        }
    }
}

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
    /// Logger
    pub logger: Logger,
}

/// Full context including P2P handle for services that submit transactions.
///
/// Note: This struct is NOT Sync due to rtrb::Producer in P2PHandle.
/// Used by: TransactionService (via Mutex or channel pattern)
pub struct RpcContext {
    /// Read-only context (can be cloned to services)
    pub read_only: ReadOnlyContext,
    /// P2P handle for broadcasting transactions
    pub p2p_handle: P2PHandle,
    /// P2P readiness flag (shared for health checks)
    pub p2p_ready: Arc<AtomicBool>,
}

impl RpcContext {
    /// Create a new RPC context.
    pub fn new(
        store: Arc<ConsensusStore>,
        pending_state: PendingStateReader,
        mempool_stats: Option<MempoolStatsReader>,
        peer_stats: Option<PeerStatsReader>,
        p2p_handle: P2PHandle,
        p2p_ready: Arc<AtomicBool>,
        logger: Logger,
    ) -> Self {
        Self {
            read_only: ReadOnlyContext {
                store,
                pending_state,
                mempool_stats,
                peer_stats,
                logger,
            },
            p2p_handle,
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
        let node_service = NodeServiceImpl::new(
            read_ctx,
            self.config.peer_id,
            self.config.network.clone(),
            self.config.total_validators,
            self.config.f,
            Arc::clone(&self.context.p2p_ready),
        );

        // Build routes with implemented services
        let routes = Routes::new(AccountServiceServer::new(account_service))
            .add_service(BlockServiceServer::new(block_service))
            .add_service(NodeServiceServer::new(node_service));

        Server::builder().serve(addr, routes).await
    }
}
