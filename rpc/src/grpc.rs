//! gRPC server for RPC nodes.
//!
//! Provides a read-only gRPC server that exposes:
//! - `ConsensusService` - L-notarization queries
//! - `BlockService` - Finalized block queries

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use consensus::storage::store::ConsensusStore;
use consensus::validation::PendingStateReader;
use slog::Logger;
use tonic::transport::Server;

use grpc_client::proto::block_service_server::BlockServiceServer;
use grpc_client::proto::consensus_service_server::ConsensusServiceServer;
use grpc_client::server::ReadOnlyContext;
use grpc_client::services::{BlockServiceImpl, ConsensusServiceImpl};

/// Configuration for the RPC node gRPC server.
#[derive(Debug, Clone)]
pub struct GrpcServerConfig {
    /// Address to listen on
    pub listen_addr: SocketAddr,
}

/// A read-only gRPC server for RPC nodes.
///
/// Unlike the full validator gRPC server, this only exposes read-only services:
/// - `ConsensusService` for L-notarization queries
/// - `BlockService` for finalized block queries
///
/// RPC nodes don't accept transactions or participate in consensus,
/// so services like `TransactionService` and `AdminService` are not included.
pub struct RpcGrpcServer {
    config: GrpcServerConfig,
    context: ReadOnlyContext,
}

impl RpcGrpcServer {
    /// Create a new RPC gRPC server.
    pub fn new(config: GrpcServerConfig, store: Arc<ConsensusStore>, logger: Logger) -> Self {
        // Create a minimal ReadOnlyContext for RPC nodes
        // RPC nodes don't have pending state, mempool, or subscriptions
        let context = ReadOnlyContext {
            store,
            pending_state: PendingStateReader::empty(),
            mempool_stats: None,
            peer_stats: None,
            block_events: None,
            consensus_events: None,
            tx_events: None,
            logger,
        };

        Self { config, context }
    }

    /// Start the gRPC server with a graceful shutdown signal.
    ///
    /// The server will stop accepting new connections when the `shutdown` future
    /// completes, then wait for existing connections to finish.
    pub async fn serve_with_shutdown<F>(self, shutdown: F) -> Result<()>
    where
        F: Future<Output = ()> + Send,
    {
        let addr = self.config.listen_addr;

        slog::info!(
            self.context.logger,
            "Starting RPC gRPC server";
            "address" => %addr,
        );

        // Create read-only service implementations
        let consensus_service = ConsensusServiceImpl::new(self.context.clone());
        let block_service = BlockServiceImpl::new(self.context.clone());

        let logger = self.context.logger.clone();

        // Build routes with read-only services only
        Server::builder()
            .add_service(ConsensusServiceServer::new(consensus_service))
            .add_service(BlockServiceServer::new(block_service))
            .serve_with_shutdown(addr, shutdown)
            .await
            .map_err(|e| anyhow::anyhow!("gRPC server error: {}", e))?;

        slog::info!(logger, "RPC gRPC server stopped");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grpc_server_config() {
        let config = GrpcServerConfig {
            listen_addr: "0.0.0.0:50051".parse().unwrap(),
        };
        assert_eq!(config.listen_addr.port(), 50051);
    }
}
