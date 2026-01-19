//! Block service implementation.

use consensus::state::block::Block;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::proto::block_service_server::BlockService;
use crate::proto::{
    BlockResponse, Empty, GetBlockByHeightRequest, GetBlockRequest, GetBlocksRequest,
    GetBlocksResponse, TransactionInfo, TransactionType,
};
use crate::server::ReadOnlyContext;

/// Implementation of the BlockService gRPC service.
pub struct BlockServiceImpl {
    context: ReadOnlyContext,
}

impl BlockServiceImpl {
    /// Create a new BlockService implementation.
    pub fn new(context: ReadOnlyContext) -> Self {
        Self { context }
    }
}

/// Convert a Block to a BlockResponse proto message.
fn block_to_response(block: &Block) -> BlockResponse {
    let transactions: Vec<TransactionInfo> = block
        .transactions
        .iter()
        .map(|tx| {
            let recipient_hex = tx
                .recipient()
                .map(|r| hex::encode(r.as_bytes()))
                .unwrap_or_default();

            TransactionInfo {
                tx_hash: hex::encode(tx.tx_hash),
                sender: hex::encode(tx.sender.as_bytes()),
                recipient: recipient_hex,
                amount: tx.amount(),
                fee: tx.fee,
                nonce: tx.nonce,
                timestamp: tx.timestamp,
                r#type: match tx.instruction {
                    consensus::state::transaction::TransactionInstruction::Transfer { .. } => {
                        TransactionType::Transfer as i32
                    }
                    consensus::state::transaction::TransactionInstruction::Mint { .. } => {
                        TransactionType::Mint as i32
                    }
                    consensus::state::transaction::TransactionInstruction::Burn { .. } => {
                        TransactionType::Burn as i32
                    }
                    consensus::state::transaction::TransactionInstruction::CreateAccount {
                        ..
                    } => TransactionType::CreateAccount as i32,
                },
            }
        })
        .collect();

    BlockResponse {
        hash: hex::encode(block.get_hash()),
        view: block.view(),
        height: block.height,
        parent_hash: hex::encode(block.parent_block_hash()),
        timestamp: block.header.timestamp,
        leader_id: block.leader,
        transactions,
        tx_count: block.transactions.len() as u32,
        is_finalized: block.is_finalized,
    }
}

#[tonic::async_trait]
impl BlockService for BlockServiceImpl {
    /// Get a specific block by hash.
    async fn get_block(
        &self,
        request: Request<GetBlockRequest>,
    ) -> Result<Response<BlockResponse>, Status> {
        let req = request.into_inner();
        let hash = parse_hash(&req.hash)?;

        // Try finalized blocks first, then non-finalized
        let block = self
            .context
            .store
            .get_finalized_block(&hash)
            .ok()
            .flatten()
            .or_else(|| {
                self.context
                    .store
                    .get_non_finalized_block(&hash)
                    .ok()
                    .flatten()
            });

        match block {
            Some(b) => Ok(Response::new(block_to_response(&b))),
            None => Err(Status::not_found("Block not found")),
        }
    }

    /// Get the latest finalized block.
    async fn get_latest_block(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<BlockResponse>, Status> {
        let blocks = self
            .context
            .store
            .get_all_finalized_blocks()
            .map_err(|e| Status::internal(format!("Database error: {}", e)))?;

        match blocks.last() {
            Some(b) => Ok(Response::new(block_to_response(b))),
            None => Err(Status::not_found("No blocks found")),
        }
    }

    /// Get block by height.
    async fn get_block_by_height(
        &self,
        request: Request<GetBlockByHeightRequest>,
    ) -> Result<Response<BlockResponse>, Status> {
        let req = request.into_inner();

        let blocks = self
            .context
            .store
            .get_all_finalized_blocks()
            .map_err(|e| Status::internal(format!("Database error: {}", e)))?;

        let block = blocks.iter().find(|b| b.height == req.height);

        match block {
            Some(b) => Ok(Response::new(block_to_response(b))),
            None => Err(Status::not_found(format!(
                "Block not found at height {}",
                req.height
            ))),
        }
    }

    /// Get a range of finalized blocks (paginated).
    async fn get_blocks(
        &self,
        request: Request<GetBlocksRequest>,
    ) -> Result<Response<GetBlocksResponse>, Status> {
        let req = request.into_inner();
        let limit = if req.limit == 0 || req.limit > 1000 {
            100
        } else {
            req.limit as usize
        };

        let all_blocks = self
            .context
            .store
            .get_all_finalized_blocks()
            .map_err(|e| Status::internal(format!("Database error: {}", e)))?;

        // Filter by height range
        let to_height = if req.to_height == 0 {
            u64::MAX
        } else {
            req.to_height
        };

        let filtered: Vec<_> = all_blocks
            .iter()
            .filter(|b| b.height >= req.from_height && b.height <= to_height)
            .collect();

        let has_more = filtered.len() > limit;
        let blocks: Vec<BlockResponse> = filtered
            .into_iter()
            .take(limit)
            .map(block_to_response)
            .collect();

        Ok(Response::new(GetBlocksResponse { blocks, has_more }))
    }

    type StreamBlocksStream = ReceiverStream<Result<BlockResponse, Status>>;

    /// Stream blocks in a range.
    async fn stream_blocks(
        &self,
        request: Request<GetBlocksRequest>,
    ) -> Result<Response<Self::StreamBlocksStream>, Status> {
        let req = request.into_inner();

        let all_blocks = self
            .context
            .store
            .get_all_finalized_blocks()
            .map_err(|e| Status::internal(format!("Database error: {}", e)))?;

        // Filter by height range
        let to_height = if req.to_height == 0 {
            u64::MAX
        } else {
            req.to_height
        };

        let filtered: Vec<_> = all_blocks
            .into_iter()
            .filter(|b| b.height >= req.from_height && b.height <= to_height)
            .collect();

        let (tx, rx) = tokio::sync::mpsc::channel(32);

        tokio::spawn(async move {
            for block in filtered {
                if tx.send(Ok(block_to_response(&block))).await.is_err() {
                    break; // Client disconnected
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

/// Parse a hex-encoded hash string into a 32-byte array.
fn parse_hash(hex_str: &str) -> Result<[u8; 32], Status> {
    let bytes = hex::decode(hex_str)
        .map_err(|e| Status::invalid_argument(format!("Invalid hex hash: {}", e)))?;

    if bytes.len() != 32 {
        return Err(Status::invalid_argument(format!(
            "Hash must be 32 bytes, got {}",
            bytes.len()
        )));
    }

    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(arr)
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
        p.push(format!("block_service_test_{}.redb", rand::random::<u64>()));
        p
    }

    fn create_test_logger() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    fn create_test_context() -> (ReadOnlyContext, Arc<ConsensusStore>, PendingStateWriter) {
        let path = temp_db_path();
        let store = Arc::new(ConsensusStore::open(path.as_path()).unwrap());
        let (writer, reader) = PendingStateWriter::new(Arc::clone(&store), 0);

        let context = ReadOnlyContext {
            store: Arc::clone(&store),
            pending_state: reader,
            mempool_stats: None,
            peer_stats: None,
            logger: create_test_logger(),
        };

        (context, store, writer)
    }

    fn create_test_transaction() -> Transaction {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let sender = Address::from_public_key(&pk);
        let recipient = Address::from_bytes([7u8; 32]);
        Transaction::new_transfer(sender, recipient, 100, 0, 10, &sk)
    }

    fn create_test_block(height: u64, view: u64, parent_hash: [u8; 32], with_txs: bool) -> Block {
        let sk = BlsSecretKey::generate(&mut rand::thread_rng());
        let leader_signature = sk.sign(b"block proposal");

        let txs = if with_txs {
            vec![Arc::new(create_test_transaction())]
        } else {
            vec![]
        };

        Block::new(
            view,
            0, // leader
            parent_hash,
            txs,
            1234567890 + height, // timestamp
            leader_signature,
            true, // is_finalized
            height,
        )
    }

    #[test]
    fn parse_hash_valid() {
        let hex = "0000000000000000000000000000000000000000000000000000000000000000";
        let result = parse_hash(hex);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), [0u8; 32]);
    }

    #[test]
    fn parse_hash_valid_nonzero() {
        let hex = "0102030405060708091011121314151617181920212223242526272829303132";
        let result = parse_hash(hex);
        assert!(result.is_ok());
    }

    #[test]
    fn parse_hash_invalid_hex() {
        let hex = "zzzz0000000000000000000000000000000000000000000000000000000000";
        let result = parse_hash(hex);
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn parse_hash_wrong_length() {
        let hex = "0102030405"; // Only 5 bytes
        let result = parse_hash(hex);
        assert!(result.is_err());
    }

    #[test]
    fn parse_hash_empty() {
        let result = parse_hash("");
        assert!(result.is_err());
    }

    #[test]
    fn block_to_response_empty_block() {
        let block = create_test_block(0, 0, [0u8; 32], false);
        let response = block_to_response(&block);

        assert_eq!(response.height, 0);
        assert_eq!(response.view, 0);
        assert_eq!(response.tx_count, 0);
        assert!(response.transactions.is_empty());
        assert!(response.is_finalized);
        assert!(!response.hash.is_empty());
    }

    #[test]
    fn block_to_response_with_transactions() {
        let block = create_test_block(5, 10, [1u8; 32], true);
        let response = block_to_response(&block);

        assert_eq!(response.height, 5);
        assert_eq!(response.view, 10);
        assert_eq!(response.tx_count, 1);
        assert_eq!(response.transactions.len(), 1);
        assert!(!response.transactions[0].tx_hash.is_empty());
        assert!(!response.transactions[0].sender.is_empty());
    }

    #[tokio::test]
    async fn get_block_not_found() {
        let (context, _store, _writer) = create_test_context();
        let service = BlockServiceImpl::new(context);

        let request = Request::new(GetBlockRequest {
            hash: "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
        });

        let result = service.get_block(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn get_block_found_finalized() {
        let (context, store, _writer) = create_test_context();

        // Create and store a block
        let block = create_test_block(1, 1, [0u8; 32], false);
        let block_hash = block.get_hash();
        store.put_finalized_block(&block).unwrap();

        let service = BlockServiceImpl::new(context);
        let request = Request::new(GetBlockRequest {
            hash: hex::encode(block_hash),
        });

        let response = service.get_block(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.height, 1);
        assert_eq!(resp.view, 1);
        assert!(resp.is_finalized);
    }

    #[tokio::test]
    async fn get_block_invalid_hash() {
        let (context, _store, _writer) = create_test_context();
        let service = BlockServiceImpl::new(context);

        let request = Request::new(GetBlockRequest {
            hash: "invalid_hex".to_string(),
        });

        let result = service.get_block(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn get_latest_block_empty_chain() {
        let (context, _store, _writer) = create_test_context();
        let service = BlockServiceImpl::new(context);

        let request = Request::new(Empty {});
        let result = service.get_latest_block(request).await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn get_latest_block_returns_highest() {
        let (context, store, _writer) = create_test_context();

        // Create multiple blocks
        let block1 = create_test_block(1, 1, [0u8; 32], false);
        let block2 = create_test_block(2, 2, block1.get_hash(), false);
        let block3 = create_test_block(3, 3, block2.get_hash(), false);

        store.put_finalized_block(&block1).unwrap();
        store.put_finalized_block(&block2).unwrap();
        store.put_finalized_block(&block3).unwrap();

        let service = BlockServiceImpl::new(context);
        let request = Request::new(Empty {});

        let response = service.get_latest_block(request).await.unwrap();
        let resp = response.into_inner();

        // get_all_finalized_blocks returns sorted by view, so last should be block3
        assert_eq!(resp.height, 3);
        assert_eq!(resp.view, 3);
    }

    #[tokio::test]
    async fn get_block_by_height_not_found() {
        let (context, _store, _writer) = create_test_context();
        let service = BlockServiceImpl::new(context);

        let request = Request::new(GetBlockByHeightRequest { height: 999 });
        let result = service.get_block_by_height(request).await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn get_block_by_height_found() {
        let (context, store, _writer) = create_test_context();

        let block1 = create_test_block(1, 1, [0u8; 32], false);
        let block2 = create_test_block(2, 2, block1.get_hash(), false);
        store.put_finalized_block(&block1).unwrap();
        store.put_finalized_block(&block2).unwrap();

        let service = BlockServiceImpl::new(context);
        let request = Request::new(GetBlockByHeightRequest { height: 2 });

        let response = service.get_block_by_height(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.height, 2);
    }

    #[tokio::test]
    async fn get_blocks_empty_range() {
        let (context, _store, _writer) = create_test_context();
        let service = BlockServiceImpl::new(context);

        let request = Request::new(GetBlocksRequest {
            from_height: 0,
            to_height: 10,
            limit: 100,
        });

        let response = service.get_blocks(request).await.unwrap();
        let resp = response.into_inner();

        assert!(resp.blocks.is_empty());
        assert!(!resp.has_more);
    }

    #[tokio::test]
    async fn get_blocks_returns_range() {
        let (context, store, _writer) = create_test_context();

        // Create blocks 1-5
        let mut prev_hash = [0u8; 32];
        for i in 1..=5 {
            let block = create_test_block(i, i, prev_hash, false);
            prev_hash = block.get_hash();
            store.put_finalized_block(&block).unwrap();
        }

        let service = BlockServiceImpl::new(context);
        let request = Request::new(GetBlocksRequest {
            from_height: 2,
            to_height: 4,
            limit: 100,
        });

        let response = service.get_blocks(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.blocks.len(), 3); // Heights 2, 3, 4
        assert!(!resp.has_more);
    }

    #[tokio::test]
    async fn get_blocks_respects_limit() {
        let (context, store, _writer) = create_test_context();

        // Create blocks 1-10
        let mut prev_hash = [0u8; 32];
        for i in 1..=10 {
            let block = create_test_block(i, i, prev_hash, false);
            prev_hash = block.get_hash();
            store.put_finalized_block(&block).unwrap();
        }

        let service = BlockServiceImpl::new(context);
        let request = Request::new(GetBlocksRequest {
            from_height: 1,
            to_height: 10,
            limit: 3,
        });

        let response = service.get_blocks(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.blocks.len(), 3);
        assert!(resp.has_more);
    }

    #[tokio::test]
    async fn get_blocks_default_limit() {
        let (context, _store, _writer) = create_test_context();
        let service = BlockServiceImpl::new(context);

        // limit = 0 should default to 100
        let request = Request::new(GetBlocksRequest {
            from_height: 0,
            to_height: 0,
            limit: 0,
        });

        let response = service.get_blocks(request).await.unwrap();
        // Just verify it doesn't error - empty result is fine
        assert!(response.into_inner().blocks.is_empty());
    }

    #[tokio::test]
    async fn stream_blocks_returns_stream() {
        let (context, store, _writer) = create_test_context();

        // Create blocks 1-3
        let mut prev_hash = [0u8; 32];
        for i in 1..=3 {
            let block = create_test_block(i, i, prev_hash, false);
            prev_hash = block.get_hash();
            store.put_finalized_block(&block).unwrap();
        }

        let service = BlockServiceImpl::new(context);
        let request = Request::new(GetBlocksRequest {
            from_height: 1,
            to_height: 3,
            limit: 0,
        });

        let response = service.stream_blocks(request).await.unwrap();
        let mut stream = response.into_inner();

        let mut count = 0;
        while let Some(result) = tokio_stream::StreamExt::next(&mut stream).await {
            assert!(result.is_ok());
            count += 1;
        }

        assert_eq!(count, 3);
    }
}
