//! Admin service implementation for administrative operations.

use tonic::{Request, Response, Status};

use crate::proto::admin_service_server::AdminService;
use crate::proto::{
    Empty, GetViewDebugInfoRequest, MetricsResponse, RefreshPeersResponse, ValidatorsResponse,
    ViewDebugInfoResponse,
};
use crate::server::ReadOnlyContext;

/// Implementation of the AdminService gRPC service.
pub struct AdminServiceImpl {
    context: ReadOnlyContext,
}

impl AdminServiceImpl {
    /// Create a new AdminService implementation.
    pub fn new(context: ReadOnlyContext) -> Self {
        Self { context }
    }
}

#[tonic::async_trait]
impl AdminService for AdminServiceImpl {
    /// Get detailed node metrics in Prometheus format.
    ///
    /// NOTE: Currently returns empty metrics. Full implementation requires
    /// integration with a metrics registry (e.g., prometheus crate).
    async fn get_metrics(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<MetricsResponse>, Status> {
        // TODO: Integrate with metrics registry when available
        Ok(Response::new(MetricsResponse {
            metrics: String::new(),
        }))
    }

    /// Get the validator set.
    ///
    /// NOTE: Currently returns empty list. Full implementation requires
    /// access to validator configuration/registry.
    async fn get_validators(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<ValidatorsResponse>, Status> {
        // TODO: Implement when validator registry is available
        Ok(Response::new(ValidatorsResponse { validators: vec![] }))
    }

    /// Manually trigger peer discovery.
    ///
    /// NOTE: Currently returns zeros. Full implementation requires
    /// P2P refresh API in P2PHandle.
    async fn refresh_peers(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<RefreshPeersResponse>, Status> {
        // TODO: Implement when P2P refresh API is available
        Ok(Response::new(RefreshPeersResponse {
            discovered_count: 0,
            connected_count: 0,
        }))
    }

    /// Get debug info for a specific view.
    async fn get_view_debug_info(
        &self,
        request: Request<GetViewDebugInfoRequest>,
    ) -> Result<Response<ViewDebugInfoResponse>, Status> {
        let req = request.into_inner();
        let view = req.view;

        // Try to find a block at this view in finalized blocks
        let blocks = self
            .context
            .store
            .get_all_finalized_blocks()
            .map_err(|e| Status::internal(format!("Database error: {}", e)))?;

        // Find block matching this view
        for block in blocks {
            if block.view() == view {
                return Ok(Response::new(ViewDebugInfoResponse {
                    view,
                    leader: block.leader,
                    has_block: true,
                    block_hash: hex::encode(block.get_hash()),
                    votes_received: 0,    // Not tracked in finalized blocks
                    is_m_notarized: true, // Finalized implies M-notarized
                    is_l_notarized: true, // We're looking at finalized blocks
                    is_nullified: false,
                }));
            }
        }

        // View not found - return empty response
        Ok(Response::new(ViewDebugInfoResponse {
            view,
            leader: 0,
            has_block: false,
            block_hash: String::new(),
            votes_received: 0,
            is_m_notarized: false,
            is_l_notarized: false,
            is_nullified: false, // Unknown - could be nullified
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    use consensus::crypto::aggregated::BlsSecretKey;
    use consensus::crypto::transaction_crypto::TxSecretKey;
    use consensus::state::address::Address;
    use consensus::state::block::Block;
    use consensus::state::transaction::Transaction;
    use consensus::storage::store::ConsensusStore;
    use consensus::validation::pending_state::PendingStateWriter;
    use slog::Logger;

    fn temp_db_path() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("admin_service_test_{}.redb", rand::random::<u64>()));
        p
    }

    fn create_test_logger() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    fn create_test_context() -> (ReadOnlyContext, Arc<ConsensusStore>) {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let context = ReadOnlyContext {
            store: Arc::clone(&store),
            pending_state: reader,
            mempool_stats: None,
            peer_stats: None,
            block_events: None,
            consensus_events: None,
            tx_events: None,
            logger: create_test_logger(),
        };

        (context, store)
    }

    fn create_test_transaction() -> Transaction {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let sender = Address::from_public_key(&pk);
        let recipient = Address::from_bytes([7u8; 32]);
        Transaction::new_transfer(sender, recipient, 100, 0, 10, &sk)
    }

    fn create_test_block(height: u64, view: u64, parent_hash: [u8; 32]) -> Block {
        let sk = BlsSecretKey::generate(&mut rand::thread_rng());
        let leader_signature = sk.sign(b"block proposal");
        let tx = Arc::new(create_test_transaction());

        Block::new(
            view,
            0, // leader
            parent_hash,
            vec![tx],
            1234567890 + height,
            leader_signature,
            true,
            height,
        )
    }

    #[tokio::test]
    async fn get_metrics_returns_empty() {
        let (context, _store) = create_test_context();
        let service = AdminServiceImpl::new(context);

        let request = Request::new(Empty {});
        let response = service.get_metrics(request).await.unwrap();
        let resp = response.into_inner();

        assert!(resp.metrics.is_empty());
    }

    #[tokio::test]
    async fn get_validators_returns_empty() {
        let (context, _store) = create_test_context();
        let service = AdminServiceImpl::new(context);

        let request = Request::new(Empty {});
        let response = service.get_validators(request).await.unwrap();
        let resp = response.into_inner();

        assert!(resp.validators.is_empty());
    }

    #[tokio::test]
    async fn refresh_peers_returns_zeros() {
        let (context, _store) = create_test_context();
        let service = AdminServiceImpl::new(context);

        let request = Request::new(Empty {});
        let response = service.refresh_peers(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.discovered_count, 0);
        assert_eq!(resp.connected_count, 0);
    }

    #[tokio::test]
    async fn get_view_debug_info_not_found() {
        let (context, _store) = create_test_context();
        let service = AdminServiceImpl::new(context);

        let request = Request::new(GetViewDebugInfoRequest { view: 999 });
        let response = service.get_view_debug_info(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.view, 999);
        assert!(!resp.has_block);
        assert!(resp.block_hash.is_empty());
    }

    #[tokio::test]
    async fn get_view_debug_info_found() {
        let (context, store) = create_test_context();

        // Create a block at view 5
        let block = create_test_block(1, 5, [0u8; 32]);
        store.put_finalized_block(&block).unwrap();

        let service = AdminServiceImpl::new(context);

        let request = Request::new(GetViewDebugInfoRequest { view: 5 });
        let response = service.get_view_debug_info(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.view, 5);
        assert!(resp.has_block);
        assert!(!resp.block_hash.is_empty());
        assert!(resp.is_m_notarized);
        assert!(resp.is_l_notarized);
    }
}
