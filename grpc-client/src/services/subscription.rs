//! Subscription service implementation for real-time event streaming.

use std::pin::Pin;

use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status};

use crate::proto::subscription_service_server::SubscriptionService;
use crate::proto::{
    BlockEvent, ConsensusEvent, Empty, SubscribeAddressRequest, SubscribeBlocksRequest,
    SubscribePendingRequest, TransactionEvent,
};
use crate::server::ReadOnlyContext;

use super::utils::parse_address;

/// Implementation of the SubscriptionService gRPC service.
pub struct SubscriptionServiceImpl {
    context: ReadOnlyContext,
}

impl SubscriptionServiceImpl {
    /// Create a new SubscriptionService implementation.
    pub fn new(context: ReadOnlyContext) -> Self {
        Self { context }
    }
}

#[tonic::async_trait]
impl SubscriptionService for SubscriptionServiceImpl {
    type SubscribeBlocksStream =
        Pin<Box<dyn Stream<Item = Result<BlockEvent, Status>> + Send + 'static>>;
    type SubscribeAddressStream =
        Pin<Box<dyn Stream<Item = Result<TransactionEvent, Status>> + Send + 'static>>;
    type SubscribePendingTransactionsStream =
        Pin<Box<dyn Stream<Item = Result<TransactionEvent, Status>> + Send + 'static>>;
    type SubscribeConsensusStream =
        Pin<Box<dyn Stream<Item = Result<ConsensusEvent, Status>> + Send + 'static>>;

    /// Subscribe to new finalized blocks.
    async fn subscribe_blocks(
        &self,
        request: Request<SubscribeBlocksRequest>,
    ) -> Result<Response<Self::SubscribeBlocksStream>, Status> {
        let _req = request.into_inner();

        let sender = self
            .context
            .block_events
            .as_ref()
            .ok_or_else(|| Status::unavailable("Block event streaming not configured"))?;

        let receiver = sender.subscribe();
        let stream = BroadcastStream::new(receiver).filter_map(|result| match result {
            Ok(event) => Some(Ok(event)),
            Err(e) => {
                // Log lag errors but continue streaming
                slog::warn!(
                    slog::Logger::root(slog::Discard, slog::o!()),
                    "Broadcast lag: {}",
                    e
                );
                None
            }
        });

        Ok(Response::new(Box::pin(stream)))
    }

    /// Subscribe to transactions for a specific address.
    async fn subscribe_address(
        &self,
        request: Request<SubscribeAddressRequest>,
    ) -> Result<Response<Self::SubscribeAddressStream>, Status> {
        let req = request.into_inner();
        let address = parse_address(&req.address)?;
        let address_bytes = address.as_bytes().to_vec();

        let sender = self
            .context
            .tx_events
            .as_ref()
            .ok_or_else(|| Status::unavailable("Transaction event streaming not configured"))?;

        let receiver = sender.subscribe();
        let stream = BroadcastStream::new(receiver).filter_map(move |result| {
            match result {
                Ok(event) => {
                    // Filter by address (sender or recipient)
                    if let Some(ref tx) = event.transaction {
                        let sender_bytes = hex::decode(&tx.sender).unwrap_or_default();
                        let recipient_bytes = hex::decode(&tx.recipient).unwrap_or_default();

                        if sender_bytes == address_bytes || recipient_bytes == address_bytes {
                            return Some(Ok(event));
                        }
                    }
                    None
                }
                Err(_) => None,
            }
        });

        Ok(Response::new(Box::pin(stream)))
    }

    /// Subscribe to new pending transactions in mempool.
    async fn subscribe_pending_transactions(
        &self,
        request: Request<SubscribePendingRequest>,
    ) -> Result<Response<Self::SubscribePendingTransactionsStream>, Status> {
        let req = request.into_inner();
        let sender_filter = if req.sender_filter.is_empty() {
            None
        } else {
            Some(parse_address(&req.sender_filter)?.as_bytes().to_vec())
        };
        let min_fee = req.min_fee;

        let sender = self
            .context
            .tx_events
            .as_ref()
            .ok_or_else(|| Status::unavailable("Transaction event streaming not configured"))?;

        let receiver = sender.subscribe();
        let stream = BroadcastStream::new(receiver).filter_map(move |result| {
            match result {
                Ok(event) => {
                    // Filter pending transactions only
                    if event.r#type != crate::proto::TransactionEventType::Submitted as i32 {
                        return None;
                    }

                    if let Some(ref tx) = event.transaction {
                        // Apply sender filter if specified
                        if let Some(ref filter) = sender_filter {
                            let sender_bytes = hex::decode(&tx.sender).unwrap_or_default();
                            if sender_bytes != *filter {
                                return None;
                            }
                        }

                        // Apply min fee filter
                        if tx.fee < min_fee {
                            return None;
                        }

                        return Some(Ok(event));
                    }
                    None
                }
                Err(_) => None,
            }
        });

        Ok(Response::new(Box::pin(stream)))
    }

    /// Subscribe to consensus events (view changes, leader elections).
    async fn subscribe_consensus(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Self::SubscribeConsensusStream>, Status> {
        let sender = self
            .context
            .consensus_events
            .as_ref()
            .ok_or_else(|| Status::unavailable("Consensus event streaming not configured"))?;

        let receiver = sender.subscribe();
        let stream = BroadcastStream::new(receiver).filter_map(|result| match result {
            Ok(event) => Some(Ok(event)),
            Err(_) => None,
        });

        Ok(Response::new(Box::pin(stream)))
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
    use tokio::sync::broadcast;

    fn temp_db_path() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "subscription_service_test_{}.redb",
            rand::random::<u64>()
        ));
        p
    }

    fn create_test_logger() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    fn create_test_context_with_channels() -> (
        ReadOnlyContext,
        broadcast::Sender<BlockEvent>,
        broadcast::Sender<ConsensusEvent>,
        broadcast::Sender<TransactionEvent>,
    ) {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let (block_tx, _) = broadcast::channel(16);
        let (consensus_tx, _) = broadcast::channel(16);
        let (tx_tx, _) = broadcast::channel(16);

        let context = ReadOnlyContext {
            store,
            pending_state: reader,
            mempool_stats: None,
            peer_stats: None,
            block_events: Some(block_tx.clone()),
            consensus_events: Some(consensus_tx.clone()),
            tx_events: Some(tx_tx.clone()),
            logger: create_test_logger(),
        };

        (context, block_tx, consensus_tx, tx_tx)
    }

    #[tokio::test]
    async fn subscribe_blocks_returns_stream() {
        let (context, block_tx, _, _) = create_test_context_with_channels();
        let service = SubscriptionServiceImpl::new(context);

        let request = Request::new(SubscribeBlocksRequest {
            from_height: 0,
            include_transactions: false,
        });

        let response = service.subscribe_blocks(request).await.unwrap();
        let mut stream = response.into_inner();

        // Send an event
        let event = BlockEvent {
            block: None,
            r#type: crate::proto::BlockEventType::Finalized as i32,
        };
        block_tx.send(event.clone()).unwrap();

        // Receive it
        let received = stream.next().await.unwrap().unwrap();
        assert_eq!(
            received.r#type,
            crate::proto::BlockEventType::Finalized as i32
        );
    }

    #[tokio::test]
    async fn subscribe_consensus_returns_stream() {
        let (context, _, consensus_tx, _) = create_test_context_with_channels();
        let service = SubscriptionServiceImpl::new(context);

        let request = Request::new(Empty {});
        let response = service.subscribe_consensus(request).await.unwrap();
        let mut stream = response.into_inner();

        // Send an event
        let event = ConsensusEvent {
            r#type: crate::proto::ConsensusEventType::ViewChange as i32,
            view: 42,
            leader: 1,
            timestamp: 12345,
            data: vec![],
        };
        consensus_tx.send(event.clone()).unwrap();

        // Receive it
        let received = stream.next().await.unwrap().unwrap();
        assert_eq!(received.view, 42);
        assert_eq!(
            received.r#type,
            crate::proto::ConsensusEventType::ViewChange as i32
        );
    }

    #[tokio::test]
    async fn subscribe_blocks_unavailable_without_channel() {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (_writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let context = ReadOnlyContext {
            store,
            pending_state: reader,
            mempool_stats: None,
            peer_stats: None,
            block_events: None,
            consensus_events: None,
            tx_events: None,
            logger: create_test_logger(),
        };

        let service = SubscriptionServiceImpl::new(context);
        let request = Request::new(SubscribeBlocksRequest {
            from_height: 0,
            include_transactions: false,
        });

        let result = service.subscribe_blocks(request).await;
        match result {
            Err(status) => assert_eq!(status.code(), tonic::Code::Unavailable),
            Ok(_) => panic!("Expected error but got success"),
        }
    }
}
