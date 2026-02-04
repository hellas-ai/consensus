//! RPC Node core implementation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use consensus::storage::store::ConsensusStore;
use slog::Logger;
use tokio::sync::Notify;

use crate::grpc::{GrpcServerConfig, RpcGrpcServer};
use crate::sync::parse_validator_peer_set;
use crate::{RpcConfig, RpcIdentity};

/// RPC Node state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcNodeState {
    /// Node is starting up.
    Starting,
    /// Node is syncing blocks from validators.
    Syncing,
    /// Node is fully synced and serving queries.
    Ready,
    /// Node is shutting down.
    ShuttingDown,
}

/// An RPC node that syncs finalized blocks and serves read-only queries.
///
/// RPC nodes do NOT participate in consensus. They:
/// - Connect to validators via P2P
/// - Receive finalized blocks with L-notarization proofs
/// - Verify block finality using aggregated BLS signatures
/// - Serve read-only gRPC queries (blocks, L-notarizations, etc.)
///
/// # Type Parameters
///
/// * `N` - Total number of validators (5f+1)
/// * `F` - Maximum faulty validators
pub struct RpcNode<const N: usize, const F: usize> {
    config: RpcConfig,
    identity: RpcIdentity,
    store: Arc<ConsensusStore>,
    logger: Logger,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<Notify>,
    state: RpcNodeState,
}

impl<const N: usize, const F: usize> RpcNode<N, F> {
    /// Create a new RPC node.
    pub fn new(config: RpcConfig, identity: RpcIdentity, logger: Logger) -> Result<Self> {
        // Ensure data directory exists
        std::fs::create_dir_all(&config.data_dir).context("Failed to create data directory")?;

        // Open consensus store (Arc for sharing with gRPC server)
        let db_path = config.data_dir.join("consensus.redb");
        let store =
            Arc::new(ConsensusStore::open(&db_path).context("Failed to open consensus store")?);

        slog::info!(logger, "RPC node initialized";
            "data_dir" => %config.data_dir.display(),
            "grpc_addr" => %config.grpc_addr,
            "p2p_addr" => %config.p2p_addr,
            "validators" => config.validators.len(),
        );

        Ok(Self {
            config,
            identity,
            store,
            logger,
            shutdown: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(Notify::new()),
            state: RpcNodeState::Starting,
        })
    }

    /// Get the current node state.
    pub fn state(&self) -> RpcNodeState {
        self.state
    }

    /// Get the consensus store.
    pub fn store(&self) -> &Arc<ConsensusStore> {
        &self.store
    }

    /// Get the node configuration.
    pub fn config(&self) -> &RpcConfig {
        &self.config
    }

    /// Get the node identity.
    pub fn identity(&self) -> &RpcIdentity {
        &self.identity
    }

    /// Signal the node to shut down.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.shutdown_notify.notify_waiters();
    }

    /// Check if shutdown has been requested.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    /// Get the shutdown signal for external triggering (e.g., Ctrl+C handler).
    pub fn get_shutdown_signal(&self) -> (Arc<AtomicBool>, Arc<Notify>) {
        (
            Arc::clone(&self.shutdown),
            Arc::clone(&self.shutdown_notify),
        )
    }

    /// Run the RPC node main loop.
    ///
    /// This will:
    /// 1. Start the gRPC server
    /// 2. Connect to validators via P2P
    /// 3. Sync finalized blocks
    /// 4. Serve queries until shutdown
    pub async fn run(&mut self) -> Result<()> {
        use crate::p2p::{BlockRequestCommand, spawn_rpc_p2p};
        use crate::sync::{
            BlockSyncer, SyncConfig, SyncState, parse_validator_keys, rpc_config_to_p2p,
        };
        use commonware_runtime::tokio::Runner as TokioRunner;

        slog::info!(self.logger, "Starting RPC node");
        self.state = RpcNodeState::Syncing;

        // Create gRPC server
        let grpc_config = GrpcServerConfig {
            listen_addr: self.config.grpc_addr,
        };
        let grpc_server =
            RpcGrpcServer::new(grpc_config, Arc::clone(&self.store), self.logger.clone());

        // Shutdown signal for gRPC server
        let grpc_shutdown = Arc::clone(&self.shutdown_notify);
        let grpc_handle = tokio::spawn(async move {
            grpc_server
                .serve_with_shutdown(async move {
                    grpc_shutdown.notified().await;
                })
                .await
        });

        slog::info!(self.logger, "gRPC server started"; "addr" => %self.config.grpc_addr);

        // Parse validator public keys from config
        let validator_keys = parse_validator_keys(&self.config.validators);
        slog::info!(self.logger, "Parsed validator keys";
            "count" => validator_keys.len()
        );

        // Create P2P config and spawn P2P service
        let p2p_config = rpc_config_to_p2p(&self.config);
        let (p2p_handle, request_tx, mut sync_rx) = spawn_rpc_p2p::<TokioRunner, 6, 1, 3>(
            TokioRunner::default(),
            p2p_config,
            self.identity.clone(),
            self.logger.clone(),
        );

        slog::info!(self.logger, "P2P service started"; "addr" => %self.config.p2p_addr);

        // Wait for P2P to be ready
        p2p_handle.wait_ready().await;
        slog::info!(self.logger, "P2P service ready");

        // Create block syncer with BLS public keys for L-notarization verification
        let peer_set = parse_validator_peer_set(&self.config.validators);
        slog::info!(self.logger, "Parsed validator BLS public keys";
            "count" => peer_set.sorted_peer_ids.len(),
            "validators_in_config" => self.config.validators.len(),
        );

        let mut syncer = BlockSyncer::<N, F>::new(
            Arc::clone(&self.store),
            validator_keys,
            peer_set,
            SyncConfig::default(),
            self.logger.clone(),
        );

        slog::info!(self.logger, "Block syncer initialized";
            "state" => ?syncer.state()
        );

        // Mark as ready when either:
        // - We're synced (following mode)
        // - No validators configured (standalone mode)
        let local_height = syncer.local_height().unwrap_or(0);
        slog::info!(self.logger, "RPC node sync status";
            "local_height" => local_height,
            "validators" => self.config.validators.len()
        );

        if self.config.validators.is_empty() || syncer.is_synced() {
            self.state = RpcNodeState::Ready;
            slog::info!(
                self.logger,
                "RPC node ready (no validators or already synced)"
            );
        }

        // Main loop: wait for shutdown or sync events
        let mut sync_interval = tokio::time::interval(std::time::Duration::from_secs(5));

        while !self.is_shutdown() {
            tokio::select! {
                _ = self.shutdown_notify.notified() => {
                    break;
                }
                // Handle incoming sync responses
                Some((sender, response)) = sync_rx.recv() => {
                    slog::debug!(self.logger, "Received block response";
                        "from" => ?sender
                    );
                    if let Err(e) = syncer.handle_block_response(response, sender) {
                        slog::warn!(self.logger, "Failed to handle block response"; "error" => %e);
                    }

                    // Update state based on syncer
                    if syncer.is_synced() {
                        self.state = RpcNodeState::Ready;
                    }
                }
                _ = sync_interval.tick() => {
                    // Try to send batch block requests for parallel fetching
                    let requests = syncer.next_block_requests();
                    if !requests.is_empty() {
                        slog::debug!(self.logger, "Sending batch block requests";
                            "count" => requests.len(),
                            "first_view" => requests.first().map(|r| r.view),
                            "last_view" => requests.last().map(|r| r.view)
                        );
                        for (i, req) in requests.into_iter().enumerate() {
                            // Round-robin across validators for load distribution
                            if let Some(target) = syncer.pick_validator_at(i) {
                                let cmd = BlockRequestCommand {
                                    target,
                                    request: req,
                                };
                                let _ = request_tx.send(cmd);
                            }
                        }
                    }

                    // Periodic sync status check
                    match syncer.state() {
                        SyncState::Discovering => {
                            slog::debug!(self.logger, "Sync: discovering latest height");
                        }
                        SyncState::Syncing { current, target } => {
                            slog::debug!(self.logger, "Sync: catching up";
                                "current" => current,
                                "target" => target
                            );
                            self.state = RpcNodeState::Syncing;
                        }
                        SyncState::Following { height } => {
                            slog::debug!(self.logger, "Sync: following"; "height" => height);
                            self.state = RpcNodeState::Ready;
                        }
                    }
                }
            }
        }

        self.state = RpcNodeState::ShuttingDown;
        slog::info!(self.logger, "RPC node shutting down");

        // Shutdown P2P service
        p2p_handle.shutdown();
        let _ = p2p_handle.join();
        slog::debug!(self.logger, "P2P service stopped");

        // Wait for gRPC server to finish
        if let Err(e) = grpc_handle.await {
            slog::warn!(self.logger, "gRPC server task error"; "error" => %e);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;
    use tempfile::tempdir;

    fn test_logger() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    #[test]
    fn test_rpc_node_creation() {
        let temp = tempdir().unwrap();
        let config = RpcConfig {
            data_dir: temp.path().to_path_buf(),
            ..Default::default()
        };
        let identity = RpcIdentity::generate(&mut OsRng);
        let logger = test_logger();

        let node = RpcNode::<6, 1>::new(config, identity, logger).unwrap();
        assert_eq!(node.state(), RpcNodeState::Starting);
    }

    #[test]
    fn test_shutdown_flag() {
        let temp = tempdir().unwrap();
        let config = RpcConfig {
            data_dir: temp.path().to_path_buf(),
            ..Default::default()
        };
        let identity = RpcIdentity::generate(&mut OsRng);
        let logger = test_logger();

        let node = RpcNode::<6, 1>::new(config, identity, logger).unwrap();
        assert!(!node.is_shutdown());
        node.shutdown();
        assert!(node.is_shutdown());
    }
}
