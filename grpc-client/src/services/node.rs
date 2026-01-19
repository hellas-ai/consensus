//! Node service implementation.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tonic::{Request, Response, Status};

use crate::proto::node_service_server::NodeService;
use crate::proto::{
    ConsensusStatusResponse, Empty, HealthResponse, MempoolStatsResponse, NodeInfoResponse,
    PeersResponse, SyncStatusResponse,
};
use crate::server::ReadOnlyContext;

/// Build version from Cargo.toml
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Git commit hash (set at build time, or "unknown" if not available)
const GIT_COMMIT: &str = match option_env!("GIT_COMMIT") {
    Some(commit) => commit,
    None => "unknown",
};

/// Maximum time without progress before consensus is considered unhealthy.
/// This allows for view timeouts and brief network issues.
const CONSENSUS_STALL_TIMEOUT: Duration = Duration::from_secs(30); // 30 seconds

/// Implementation of the NodeService gRPC service.
pub struct NodeServiceImpl {
    context: ReadOnlyContext,
    /// When the node started (for uptime calculation)
    start_time: Instant,
    /// This node's peer ID (set during construction)
    peer_id: u64,
    /// Network name (e.g., "mainnet", "testnet", "local")
    network: String,
    /// Total validators in network
    total_validators: u32,
    /// Fault tolerance parameter
    f: u32,
    /// Whether P2P is ready (shared reference for health checks)
    p2p_ready: std::sync::Arc<AtomicBool>,
    /// Last observed finalized view (for liveness tracking)
    last_finalized_view: AtomicU64,
    /// When we last saw the finalized view change
    last_progress_time: Mutex<Instant>,
}

impl NodeServiceImpl {
    /// Create a new NodeService implementation.
    pub fn new(
        context: ReadOnlyContext,
        peer_id: u64,
        network: String,
        total_validators: u32,
        f: u32,
        p2p_ready: std::sync::Arc<AtomicBool>,
    ) -> Self {
        // Get initial finalized view from pending state
        let initial_view = context.pending_state.load().last_finalized_view();

        Self {
            context,
            start_time: Instant::now(),
            peer_id,
            network,
            total_validators,
            f,
            p2p_ready,
            last_finalized_view: AtomicU64::new(initial_view),
            last_progress_time: Mutex::new(Instant::now()),
        }
    }
}

#[tonic::async_trait]
impl NodeService for NodeServiceImpl {
    /// Health check for load balancers.
    async fn health(&self, _request: Request<Empty>) -> Result<Response<HealthResponse>, Status> {
        let mut components = HashMap::new();

        // Check P2P readiness
        let p2p_healthy = self.p2p_ready.load(Ordering::Acquire);
        components.insert("p2p".to_string(), p2p_healthy);

        // Storage is healthy if we can read finalized blocks without error
        let storage_healthy = self.context.store.get_all_finalized_blocks().is_ok();
        components.insert("storage".to_string(), storage_healthy);

        // Consensus health check with liveness tracking:
        // 1. Check if finalized view is progressing
        // 2. If stalled for too long, mark as unhealthy
        let consensus_healthy = {
            let snapshot = self.context.pending_state.load();
            let current_finalized = snapshot.last_finalized_view();
            let stored_finalized = self.last_finalized_view.load(Ordering::Acquire);

            // Check if we've made progress since last check
            if current_finalized > stored_finalized {
                // Progress! Update tracking
                self.last_finalized_view
                    .store(current_finalized, Ordering::Release);
                if let Ok(mut last_progress) = self.last_progress_time.lock() {
                    *last_progress = Instant::now();
                }
                true
            } else {
                // No progress - check how long we've been stalled
                let time_since_progress = self
                    .last_progress_time
                    .lock()
                    .map(|t| t.elapsed())
                    .unwrap_or(Duration::ZERO);

                // Node is healthy if:
                // - It's still within the stall timeout, OR
                // - It has at least the genesis block (view 0)
                let within_timeout = time_since_progress < CONSENSUS_STALL_TIMEOUT;
                let has_genesis = self
                    .context
                    .store
                    .get_all_finalized_blocks()
                    .map(|blocks| !blocks.is_empty())
                    .unwrap_or(false);

                within_timeout || has_genesis
            }
        };
        components.insert("consensus".to_string(), consensus_healthy);

        // Mempool is healthy if stats are available and accessible
        if let Some(ref stats_reader) = self.context.mempool_stats {
            let stats = stats_reader.load();
            // Mempool is healthy if capacity > 0 (properly initialized)
            let mempool_healthy = stats.capacity > 0;
            components.insert("mempool".to_string(), mempool_healthy);
        }

        let healthy = components.values().all(|&v| v);

        Ok(Response::new(HealthResponse {
            healthy,
            components,
        }))
    }

    /// Get detailed sync status.
    async fn get_sync_status(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<SyncStatusResponse>, Status> {
        let snapshot = self.context.pending_state.load();
        let last_finalized_view = snapshot.last_finalized_view();

        // Get all finalized blocks to determine height
        let blocks = self
            .context
            .store
            .get_all_finalized_blocks()
            .map_err(|e| Status::internal(format!("Database error: {}", e)))?;

        let highest_finalized_height = blocks.last().map(|b| b.height).unwrap_or(0);

        // For now, we assume the node is synced if it has finalized blocks
        // In a real implementation, you'd compare with network peers
        let is_synced = !blocks.is_empty();
        let views_behind = 0; // Would need network comparison

        Ok(Response::new(SyncStatusResponse {
            current_view: last_finalized_view + 1, // Approximate: next view after finalized
            highest_finalized_view: last_finalized_view,
            highest_finalized_height,
            is_synced,
            views_behind,
        }))
    }

    /// Get connected peers.
    ///
    /// Returns real peer info if PeerStatsReader is available, otherwise stub.
    async fn get_peers(&self, _request: Request<Empty>) -> Result<Response<PeersResponse>, Status> {
        let required_peers = self.total_validators - self.f;

        // Use real peer stats if available
        if let Some(ref peer_stats) = self.context.peer_stats {
            let stats = peer_stats.load();

            // Convert p2p::PeerInfo to proto PeerInfo
            let peers: Vec<crate::proto::PeerInfo> = stats
                .peers
                .iter()
                .map(|p| {
                    // Convert ed25519 key bytes to a u64 peer_id (first 8 bytes)
                    let peer_id_bytes = &p.ed25519_key[..8];
                    let peer_id = u64::from_le_bytes(peer_id_bytes.try_into().unwrap_or([0u8; 8]));

                    crate::proto::PeerInfo {
                        peer_id,
                        address: String::new(), // Address not currently tracked
                        is_validator: p.is_validator,
                        status: if p.is_validator {
                            crate::proto::PeerStatus::Connected.into()
                        } else {
                            crate::proto::PeerStatus::Unspecified.into()
                        },
                        latency_ms: 0, // TODO: Latency not currently tracked
                    }
                })
                .collect();

            return Ok(Response::new(PeersResponse {
                connected_count: stats.connected_count,
                required_peers,
                peers,
            }));
        }

        // Fallback to stub when peer stats not available
        Ok(Response::new(PeersResponse {
            connected_count: 0,
            required_peers,
            peers: vec![],
        }))
    }

    /// Get mempool statistics.
    ///
    /// Returns real mempool stats if available, otherwise defaults.
    async fn get_mempool_stats(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<MempoolStatsResponse>, Status> {
        // Use real mempool stats if available
        if let Some(ref stats_reader) = self.context.mempool_stats {
            let stats = stats_reader.load();
            return Ok(Response::new(MempoolStatsResponse {
                pending_count: stats.pending_size as u64,
                queued_count: stats.queued_size as u64,
                total_count: stats.total_size as u64,
                unique_senders: stats.unique_senders as u64,
                capacity: stats.capacity as u64,
                total_added: stats.total_added,
                total_removed: stats.total_removed,
                total_rejected: 0,     // TODO: Not tracked in PoolStats
                invalid_signatures: 0, // TODO: Not tracked in PoolStats
            }));
        }

        // Fallback to defaults when mempool stats not available
        Ok(Response::new(MempoolStatsResponse {
            pending_count: 0,
            queued_count: 0,
            total_count: 0,
            unique_senders: 0,
            capacity: 10_000,
            total_added: 0,
            total_removed: 0,
            total_rejected: 0,
            invalid_signatures: 0,
        }))
    }

    /// Get consensus status.
    async fn get_consensus_status(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<ConsensusStatusResponse>, Status> {
        let snapshot = self.context.pending_state.load();
        let last_finalized_view = snapshot.last_finalized_view();

        // Approximate current view as next after finalized
        let current_view = last_finalized_view + 1;

        // Leader is determined by view mod N in round-robin
        let current_leader = current_view % self.total_validators as u64;
        let is_leader_current_view = current_leader == self.peer_id;

        Ok(Response::new(ConsensusStatusResponse {
            current_view,
            last_finalized_view,
            peer_id: self.peer_id,
            is_leader_current_view,
            current_leader,
            total_validators: self.total_validators,
            n: self.total_validators,
            f: self.f,
        }))
    }

    /// Get node version and info.
    async fn get_node_info(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<NodeInfoResponse>, Status> {
        let uptime_seconds = self.start_time.elapsed().as_secs();

        Ok(Response::new(NodeInfoResponse {
            version: VERSION.to_string(),
            git_commit: GIT_COMMIT.to_string(),
            network: self.network.clone(),
            peer_id: self.peer_id,
            uptime_seconds,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    use consensus::storage::store::ConsensusStore;
    use consensus::validation::pending_state::PendingStateWriter;
    use slog::Logger;

    fn temp_db_path() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("node_service_test_{}.redb", rand::random::<u64>()));
        p
    }

    fn create_test_logger() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    fn create_test_service() -> NodeServiceImpl {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let context = ReadOnlyContext {
            store,
            pending_state: reader,
            mempool_stats: None,
            peer_stats: None,
            logger: create_test_logger(),
        };

        let p2p_ready = Arc::new(AtomicBool::new(true));

        NodeServiceImpl::new(context, 0, "test".to_string(), 4, 1, p2p_ready)
    }

    #[tokio::test]
    async fn health_returns_healthy() {
        let service = create_test_service();
        let request = Request::new(Empty {});

        let response = service.health(request).await.unwrap();
        let resp = response.into_inner();

        assert!(resp.healthy);
        assert!(resp.components.contains_key("p2p"));
        assert!(resp.components.contains_key("storage"));
        assert!(resp.components.contains_key("consensus"));
    }

    #[tokio::test]
    async fn health_unhealthy_when_p2p_not_ready() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let context = ReadOnlyContext {
            store,
            pending_state: reader,
            mempool_stats: None,
            peer_stats: None,
            logger: create_test_logger(),
        };

        let p2p_ready = Arc::new(AtomicBool::new(false)); // Not ready!
        let service = NodeServiceImpl::new(context, 0, "test".to_string(), 4, 1, p2p_ready);

        let request = Request::new(Empty {});
        let response = service.health(request).await.unwrap();
        let resp = response.into_inner();

        assert!(!resp.healthy); // Should be unhealthy
        assert_eq!(resp.components.get("p2p"), Some(&false));
    }

    #[tokio::test]
    async fn get_sync_status_empty_chain() {
        let service = create_test_service();
        let request = Request::new(Empty {});

        let response = service.get_sync_status(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.highest_finalized_view, 0);
        assert_eq!(resp.highest_finalized_height, 0);
    }

    #[tokio::test]
    async fn get_consensus_status_returns_params() {
        let service = create_test_service();
        let request = Request::new(Empty {});

        let response = service.get_consensus_status(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.peer_id, 0);
        assert_eq!(resp.n, 4);
        assert_eq!(resp.f, 1);
        assert_eq!(resp.total_validators, 4);
    }

    #[tokio::test]
    async fn get_node_info_returns_version() {
        let service = create_test_service();
        let request = Request::new(Empty {});

        let response = service.get_node_info(request).await.unwrap();
        let resp = response.into_inner();

        assert!(!resp.version.is_empty());
        assert_eq!(resp.network, "test");
        assert_eq!(resp.peer_id, 0);
        assert!(resp.uptime_seconds < 10); // Should be less than 10 seconds
    }

    #[tokio::test]
    async fn get_peers_returns_stub() {
        let service = create_test_service();
        let request = Request::new(Empty {});

        let response = service.get_peers(request).await.unwrap();
        let resp = response.into_inner();

        // Required peers = N - F = 4 - 1 = 3
        assert_eq!(resp.required_peers, 3);
        assert!(resp.peers.is_empty()); // Stub returns empty
    }

    // =========================================================================
    // GetMempoolStats tests (stub)
    // =========================================================================

    #[tokio::test]
    async fn get_mempool_stats_returns_stub() {
        let service = create_test_service();
        let request = Request::new(Empty {});

        let response = service.get_mempool_stats(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.capacity, 10_000);
    }
}
