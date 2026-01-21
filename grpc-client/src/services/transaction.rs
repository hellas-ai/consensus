//! Transaction service implementation.

use std::sync::{Arc, Mutex};

use consensus::state::transaction::{Transaction, TransactionError, TransactionInstruction};
use p2p::service::P2PHandle;
use tonic::{Request, Response, Status, Streaming};

use crate::proto::transaction_service_server::TransactionService;
use crate::proto::{
    AddressRole, ErrorCode, GetTransactionRequest, GetTransactionResponse,
    GetTransactionsByAddressRequest, GetTransactionsByAddressResponse, SubmitTransactionRequest,
    SubmitTransactionResponse, TransactionInfo, TransactionStatus, TransactionStatusResponse,
    TransactionType,
};
use crate::server::ReadOnlyContext;

use super::utils::{parse_address, parse_hash};

/// Implementation of the TransactionService gRPC service.
pub struct TransactionServiceImpl {
    context: ReadOnlyContext,
    /// P2P handle for broadcasting transactions (wrapped in Mutex for thread safety)
    p2p_handle: Arc<Mutex<P2PHandle>>,
}

impl TransactionServiceImpl {
    /// Create a new TransactionService implementation.
    pub fn new(context: ReadOnlyContext, p2p_handle: Arc<Mutex<P2PHandle>>) -> Self {
        Self {
            context,
            p2p_handle,
        }
    }

    /// Deserialize a transaction from bytes safely, preserving the original tx_hash.
    ///
    /// This uses `try_deserialize_preserving_hash` which keeps the original tx_hash
    /// from the serialized transaction content, allowing signature verification to work.
    fn deserialize_transaction(bytes: &[u8]) -> Result<Transaction, TransactionError> {
        Transaction::try_deserialize_preserving_hash(bytes)
    }

    /// Convert a Transaction to a TransactionInfo proto message.
    fn tx_to_info(tx: &Transaction) -> TransactionInfo {
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
                TransactionInstruction::Transfer { .. } => TransactionType::Transfer as i32,
                TransactionInstruction::Mint { .. } => TransactionType::Mint as i32,
                TransactionInstruction::Burn { .. } => TransactionType::Burn as i32,
                TransactionInstruction::CreateAccount { .. } => {
                    TransactionType::CreateAccount as i32
                }
            },
        }
    }

    /// Submit a single transaction and return the result.
    fn submit_single_transaction(&self, transaction_bytes: &[u8]) -> SubmitTransactionResponse {
        // 1. Deserialize transaction from bytes
        let tx = match Self::deserialize_transaction(transaction_bytes) {
            Ok(tx) => tx,
            Err(e) => {
                return SubmitTransactionResponse {
                    success: false,
                    tx_hash: String::new(),
                    error_message: e.to_string(),
                    error_code: ErrorCode::InvalidFormat as i32,
                };
            }
        };

        // 2. Verify signature
        if !tx.verify() {
            return SubmitTransactionResponse {
                success: false,
                tx_hash: String::new(),
                error_message: "Invalid transaction signature".to_string(),
                error_code: ErrorCode::InvalidSignature as i32,
            };
        }

        let tx_hash = hex::encode(tx.tx_hash);

        // 3. Broadcast via P2P
        {
            let mut handle = match self.p2p_handle.lock() {
                Ok(h) => h,
                Err(_) => {
                    return SubmitTransactionResponse {
                        success: false,
                        tx_hash,
                        error_message: "Failed to acquire P2P handle lock".to_string(),
                        error_code: ErrorCode::Internal as i32,
                    };
                }
            };

            if let Err(e) = handle.broadcast_transaction(tx.clone()) {
                slog::warn!(
                    self.context.logger,
                    "Failed to broadcast transaction";
                    "tx_hash" => &tx_hash,
                    "error" => %e
                );
                return SubmitTransactionResponse {
                    success: false,
                    tx_hash,
                    error_message: format!("Failed to broadcast transaction: {}", e),
                    error_code: ErrorCode::BroadcastFailed as i32,
                };
            }
        }

        slog::info!(
            self.context.logger,
            "Transaction submitted";
            "tx_hash" => &tx_hash
        );

        SubmitTransactionResponse {
            success: true,
            tx_hash,
            error_message: String::new(),
            error_code: ErrorCode::Unspecified as i32,
        }
    }

    /// Find a transaction in finalized blocks.
    /// Returns (transaction, block_hash, block_height, tx_index) if found.
    fn find_tx_in_finalized_blocks(
        &self,
        tx_hash: &[u8; 32],
    ) -> Option<(Transaction, [u8; 32], u64, u32)> {
        let blocks = self.context.store.get_all_finalized_blocks().ok()?;

        for block in blocks {
            for (idx, tx) in block.transactions.iter().enumerate() {
                if tx.tx_hash == *tx_hash {
                    return Some((
                        (*tx.clone()).clone(),
                        block.get_hash(),
                        block.height,
                        idx as u32,
                    ));
                }
            }
        }

        None
    }

    /// Find a transaction in non-finalized (M-notarized) blocks.
    /// Returns (transaction, block_hash, block_height, tx_index) if found.
    ///
    /// Note: This currently returns None as we don't have a way to enumerate
    /// all non-finalized blocks. In a full implementation, this would query
    /// the consensus manager for M-notarized but not L-notarized blocks.
    fn find_tx_in_non_finalized_blocks(
        &self,
        _tx_hash: &[u8; 32],
    ) -> Option<(Transaction, [u8; 32], u64, u32)> {
        // TODO: Implement once we have a way to enumerate non-finalized blocks.
        // The pending state tracks account diffs but not the original blocks.
        // To fully implement this, we would need:
        // 1. A separate index of M-notarized block hashes, OR
        // 2. Access to the consensus manager's view chain
        //
        // For now, transactions will show as "pending in mempool" until finalized.
        None
    }
}

#[tonic::async_trait]
impl TransactionService for TransactionServiceImpl {
    /// Submit a single signed transaction.
    /// Validates signature, adds to mempool, and broadcasts via P2P.
    async fn submit_transaction(
        &self,
        request: Request<SubmitTransactionRequest>,
    ) -> Result<Response<SubmitTransactionResponse>, Status> {
        let req = request.into_inner();
        let response = self.submit_single_transaction(&req.transaction_bytes);
        Ok(Response::new(response))
    }

    /// Submit multiple transactions in a stream.
    /// Useful for batch operations or high-throughput clients.
    async fn submit_transaction_stream(
        &self,
        request: Request<Streaming<SubmitTransactionRequest>>,
    ) -> Result<Response<SubmitTransactionResponse>, Status> {
        let mut stream = request.into_inner();
        let mut total_success = 0u32;
        let mut total_failed = 0u32;
        let mut last_tx_hash = String::new();
        let mut last_error_message = String::new();
        let mut last_error_code = ErrorCode::Unspecified as i32;

        while let Some(req) = stream.message().await? {
            let response = self.submit_single_transaction(&req.transaction_bytes);

            if response.success {
                total_success += 1;
                last_tx_hash = response.tx_hash;
            } else {
                total_failed += 1;
                last_error_message = response.error_message;
                last_error_code = response.error_code;
            }
        }

        // Return summary response
        let success = total_failed == 0 && total_success > 0;
        let error_message = if total_failed > 0 {
            format!(
                "Processed {} transactions: {} succeeded, {} failed. Last error: {}",
                total_success + total_failed,
                total_success,
                total_failed,
                last_error_message
            )
        } else {
            String::new()
        };

        Ok(Response::new(SubmitTransactionResponse {
            success,
            tx_hash: last_tx_hash,
            error_message,
            error_code: if success {
                ErrorCode::Unspecified as i32
            } else {
                last_error_code
            },
        }))
    }

    /// Get transaction by hash.
    async fn get_transaction(
        &self,
        request: Request<GetTransactionRequest>,
    ) -> Result<Response<GetTransactionResponse>, Status> {
        let req = request.into_inner();
        let tx_hash = parse_hash(&req.tx_hash)?;

        // 1. Check finalized blocks first (most likely location)
        if let Some((tx, block_hash, block_height, tx_index)) =
            self.find_tx_in_finalized_blocks(&tx_hash)
        {
            return Ok(Response::new(GetTransactionResponse {
                transaction: Some(Self::tx_to_info(&tx)),
                block_hash: hex::encode(block_hash),
                block_height,
                tx_index,
            }));
        }

        // 2. Check non-finalized (M-notarized) blocks
        if let Some((tx, block_hash, block_height, tx_index)) =
            self.find_tx_in_non_finalized_blocks(&tx_hash)
        {
            return Ok(Response::new(GetTransactionResponse {
                transaction: Some(Self::tx_to_info(&tx)),
                block_hash: hex::encode(block_hash),
                block_height,
                tx_index,
            }));
        }

        // 3. Check mempool (stored transactions)
        if let Ok(Some(tx)) = self.context.store.get_transaction(&tx_hash) {
            return Ok(Response::new(GetTransactionResponse {
                transaction: Some(Self::tx_to_info(&tx)),
                block_hash: String::new(), // Not in a block yet
                block_height: 0,
                tx_index: 0,
            }));
        }

        Err(Status::not_found(format!(
            "Transaction {} not found",
            req.tx_hash
        )))
    }

    /// Get transaction status (pending, finalized, not found).
    async fn get_transaction_status(
        &self,
        request: Request<GetTransactionRequest>,
    ) -> Result<Response<TransactionStatusResponse>, Status> {
        let req = request.into_inner();
        let tx_hash = parse_hash(&req.tx_hash)?;

        // Get current finalized view for confirmation calculation
        let snapshot = self.context.pending_state.load();
        let current_finalized_view = snapshot.last_finalized_view();

        // 1. Check finalized blocks first
        if let Some((_tx, block_hash, block_height, _tx_index)) =
            self.find_tx_in_finalized_blocks(&tx_hash)
        {
            // Get the block to determine its view for confirmation count
            let block = self
                .context
                .store
                .get_finalized_block(&block_hash)
                .ok()
                .flatten();
            let block_view = block.map(|b| b.view()).unwrap_or(0);
            let confirmations = current_finalized_view.saturating_sub(block_view);

            return Ok(Response::new(TransactionStatusResponse {
                status: TransactionStatus::Finalized as i32,
                block_hash: hex::encode(block_hash),
                block_height,
                confirmations,
            }));
        }

        // 2. Check non-finalized (M-notarized) blocks
        if let Some((_tx, block_hash, block_height, _tx_index)) =
            self.find_tx_in_non_finalized_blocks(&tx_hash)
        {
            return Ok(Response::new(TransactionStatusResponse {
                status: TransactionStatus::PendingBlock as i32,
                block_hash: hex::encode(block_hash),
                block_height,
                confirmations: 0,
            }));
        }

        // 3. Check mempool
        if self
            .context
            .store
            .get_transaction(&tx_hash)
            .ok()
            .flatten()
            .is_some()
        {
            return Ok(Response::new(TransactionStatusResponse {
                status: TransactionStatus::PendingMempool as i32,
                block_hash: String::new(),
                block_height: 0,
                confirmations: 0,
            }));
        }

        // Not found anywhere
        Ok(Response::new(TransactionStatusResponse {
            status: TransactionStatus::NotFound as i32,
            block_hash: String::new(),
            block_height: 0,
            confirmations: 0,
        }))
    }

    /// Get transactions for an address (as sender or recipient).
    async fn get_transactions_by_address(
        &self,
        request: Request<GetTransactionsByAddressRequest>,
    ) -> Result<Response<GetTransactionsByAddressResponse>, Status> {
        let req = request.into_inner();
        let address = parse_address(&req.address)?;
        let address_bytes = address.as_bytes();

        let role = AddressRole::try_from(req.role).unwrap_or(AddressRole::Unspecified);
        let limit = if req.limit == 0 || req.limit > 100 {
            100
        } else {
            req.limit as usize
        };
        let from_height = req.from_height;
        let to_height = if req.to_height == 0 {
            u64::MAX
        } else {
            req.to_height
        };

        // Parse cursor if provided (format: "height:tx_index")
        let (cursor_height, cursor_tx_index) = if !req.cursor.is_empty() {
            let parts: Vec<&str> = req.cursor.split(':').collect();
            if parts.len() == 2 {
                let h = parts[0].parse::<u64>().unwrap_or(0);
                let i = parts[1].parse::<u32>().unwrap_or(0);
                (h, i)
            } else {
                (0, 0)
            }
        } else {
            (0, 0)
        };

        let mut results = Vec::new();
        let mut has_more = false;
        let mut next_cursor = String::new();

        // Get all finalized blocks within height range
        let blocks = self
            .context
            .store
            .get_all_finalized_blocks()
            .map_err(|e| Status::internal(format!("Database error: {}", e)))?;

        // Filter blocks by height range and process transactions
        for block in blocks.iter() {
            if block.height < from_height || block.height > to_height {
                continue;
            }

            // Skip blocks before cursor
            if block.height < cursor_height {
                continue;
            }

            for (idx, tx) in block.transactions.iter().enumerate() {
                let tx_index = idx as u32;

                // Skip transactions before cursor within the same block
                if block.height == cursor_height && tx_index <= cursor_tx_index {
                    continue;
                }

                // Check if transaction matches the address filter
                let is_sender = tx.sender.as_bytes() == address_bytes;
                let is_recipient = tx
                    .recipient()
                    .map(|r| r.as_bytes() == address_bytes)
                    .unwrap_or(false);

                let matches = match role {
                    AddressRole::Sender => is_sender,
                    AddressRole::Recipient => is_recipient,
                    AddressRole::Unspecified => is_sender || is_recipient,
                };

                if matches {
                    if results.len() >= limit {
                        has_more = true;
                        next_cursor = format!("{}:{}", block.height, tx_index);
                        break;
                    }

                    results.push(GetTransactionResponse {
                        transaction: Some(Self::tx_to_info(tx)),
                        block_hash: hex::encode(block.get_hash()),
                        block_height: block.height,
                        tx_index,
                    });
                }
            }

            if has_more {
                break;
            }
        }

        Ok(Response::new(GetTransactionsByAddressResponse {
            transactions: results,
            next_cursor,
            has_more,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use consensus::crypto::aggregated::BlsSecretKey;
    use consensus::crypto::transaction_crypto::TxSecretKey;
    use consensus::state::address::Address;
    use consensus::state::block::Block;
    use consensus::storage::store::ConsensusStore;
    use consensus::validation::pending_state::PendingStateWriter;
    use rtrb::RingBuffer;
    use slog::Logger;
    use tokio::sync::Notify;

    fn temp_db_path() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "transaction_service_test_{}.redb",
            rand::random::<u64>()
        ));
        p
    }

    fn create_test_logger() -> Logger {
        Logger::root(slog::Discard, slog::o!())
    }

    fn create_test_p2p_handle() -> P2PHandle {
        let (tx_broadcast_producer, _tx_broadcast_consumer) = RingBuffer::<Transaction>::new(1000);

        P2PHandle {
            thread_handle: std::thread::spawn(|| {}),
            shutdown: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(Notify::new()),
            broadcast_notify: Arc::new(Notify::new()),
            tx_broadcast_producer,
            tx_broadcast_notify: Arc::new(Notify::new()),
            ready_notify: Arc::new(Notify::new()),
            is_ready: Arc::new(AtomicBool::new(true)),
        }
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
            block_events: None,
            consensus_events: None,
            tx_events: None,
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

    fn create_test_block(
        height: u64,
        view: u64,
        parent_hash: [u8; 32],
        txs: Vec<Arc<Transaction>>,
    ) -> Block {
        let sk = BlsSecretKey::generate(&mut rand::thread_rng());
        let leader_signature = sk.sign(b"block proposal");

        Block::new(
            view,
            0, // leader
            parent_hash,
            txs,
            1234567890 + height,
            leader_signature,
            true,
            height,
        )
    }

    #[test]
    fn deserialize_transaction_empty_bytes() {
        let result = TransactionServiceImpl::deserialize_transaction(&[]);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), TransactionError::EmptyBytes);
    }

    #[test]
    fn deserialize_transaction_invalid_bytes() {
        let result = TransactionServiceImpl::deserialize_transaction(&[1, 2, 3, 4, 5]);
        assert!(result.is_err());
    }

    #[test]
    fn tx_to_info_transfer() {
        let tx = create_test_transaction();
        let info = TransactionServiceImpl::tx_to_info(&tx);

        assert_eq!(info.tx_hash, hex::encode(tx.tx_hash));
        assert_eq!(info.sender, hex::encode(tx.sender.as_bytes()));
        assert_eq!(info.amount, tx.amount());
        assert_eq!(info.fee, tx.fee);
        assert_eq!(info.nonce, tx.nonce);
        assert_eq!(info.r#type, TransactionType::Transfer as i32);
    }

    #[test]
    fn tx_to_info_mint() {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let sender = Address::from_public_key(&pk);
        let recipient = Address::from_bytes([5u8; 32]);
        let tx = Transaction::new_mint(sender, recipient, 1000, 0, &sk);

        let info = TransactionServiceImpl::tx_to_info(&tx);
        assert_eq!(info.r#type, TransactionType::Mint as i32);
    }

    #[test]
    fn tx_to_info_create_account() {
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let sender = Address::from_public_key(&pk);
        let new_address = Address::from_bytes([5u8; 32]);
        let tx = Transaction::new_create_account(sender, new_address, 0, 10, &sk);

        let info = TransactionServiceImpl::tx_to_info(&tx);
        assert_eq!(info.r#type, TransactionType::CreateAccount as i32);
    }

    #[tokio::test]
    async fn submit_transaction_empty_bytes() {
        let (context, _store, _writer) = create_test_context();
        let p2p = Arc::new(Mutex::new(create_test_p2p_handle()));
        let service = TransactionServiceImpl::new(context, p2p);

        let request = Request::new(SubmitTransactionRequest {
            transaction_bytes: vec![],
        });

        let response = service.submit_transaction(request).await.unwrap();
        let resp = response.into_inner();

        assert!(!resp.success);
        assert_eq!(resp.error_code, ErrorCode::InvalidFormat as i32);
        assert!(resp.error_message.contains("empty"));
    }

    #[tokio::test]
    async fn submit_transaction_invalid_bytes() {
        let (context, _store, _writer) = create_test_context();
        let p2p = Arc::new(Mutex::new(create_test_p2p_handle()));
        let service = TransactionServiceImpl::new(context, p2p);

        let request = Request::new(SubmitTransactionRequest {
            transaction_bytes: vec![1, 2, 3, 4, 5],
        });

        let response = service.submit_transaction(request).await.unwrap();
        let resp = response.into_inner();

        assert!(!resp.success);
        assert_eq!(resp.error_code, ErrorCode::InvalidFormat as i32);
    }

    #[tokio::test]
    async fn get_transaction_not_found() {
        let (context, _store, _writer) = create_test_context();
        let p2p = Arc::new(Mutex::new(create_test_p2p_handle()));
        let service = TransactionServiceImpl::new(context, p2p);

        let request = Request::new(GetTransactionRequest {
            tx_hash: "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
        });

        let result = service.get_transaction(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn get_transaction_invalid_hash() {
        let (context, _store, _writer) = create_test_context();
        let p2p = Arc::new(Mutex::new(create_test_p2p_handle()));
        let service = TransactionServiceImpl::new(context, p2p);

        let request = Request::new(GetTransactionRequest {
            tx_hash: "invalid_hash".to_string(),
        });

        let result = service.get_transaction(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn get_transaction_from_finalized_block() {
        let (context, store, _writer) = create_test_context();

        // Create a transaction and put it in a finalized block
        let tx = Arc::new(create_test_transaction());
        let tx_hash = tx.tx_hash;
        let block = create_test_block(1, 1, [0u8; 32], vec![tx.clone()]);
        store.put_finalized_block(&block).unwrap();

        let p2p = Arc::new(Mutex::new(create_test_p2p_handle()));
        let service = TransactionServiceImpl::new(context, p2p);

        let request = Request::new(GetTransactionRequest {
            tx_hash: hex::encode(tx_hash),
        });

        let response = service.get_transaction(request).await.unwrap();
        let resp = response.into_inner();

        assert!(resp.transaction.is_some());
        assert_eq!(resp.block_height, 1);
        assert_eq!(resp.tx_index, 0);
        assert!(!resp.block_hash.is_empty());
    }

    #[tokio::test]
    async fn get_transaction_from_mempool() {
        let (context, store, _writer) = create_test_context();

        // Store a transaction directly in mempool
        let tx = create_test_transaction();
        let tx_hash = tx.tx_hash;
        store.put_transaction(&tx).unwrap();

        let p2p = Arc::new(Mutex::new(create_test_p2p_handle()));
        let service = TransactionServiceImpl::new(context, p2p);

        let request = Request::new(GetTransactionRequest {
            tx_hash: hex::encode(tx_hash),
        });

        let response = service.get_transaction(request).await.unwrap();
        let resp = response.into_inner();

        assert!(resp.transaction.is_some());
        assert!(resp.block_hash.is_empty()); // Not in a block
        assert_eq!(resp.block_height, 0);
    }

    #[tokio::test]
    async fn get_transaction_status_not_found() {
        let (context, _store, _writer) = create_test_context();
        let p2p = Arc::new(Mutex::new(create_test_p2p_handle()));
        let service = TransactionServiceImpl::new(context, p2p);

        let request = Request::new(GetTransactionRequest {
            tx_hash: "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
        });

        let response = service.get_transaction_status(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.status, TransactionStatus::NotFound as i32);
    }

    #[tokio::test]
    async fn get_transaction_status_finalized() {
        let (context, store, _writer) = create_test_context();

        // Create a transaction in a finalized block
        let tx = Arc::new(create_test_transaction());
        let tx_hash = tx.tx_hash;
        let block = create_test_block(1, 1, [0u8; 32], vec![tx.clone()]);
        store.put_finalized_block(&block).unwrap();

        let p2p = Arc::new(Mutex::new(create_test_p2p_handle()));
        let service = TransactionServiceImpl::new(context, p2p);

        let request = Request::new(GetTransactionRequest {
            tx_hash: hex::encode(tx_hash),
        });

        let response = service.get_transaction_status(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.status, TransactionStatus::Finalized as i32);
        assert_eq!(resp.block_height, 1);
        assert!(!resp.block_hash.is_empty());
    }

    #[tokio::test]
    async fn get_transaction_status_pending_mempool() {
        let (context, store, _writer) = create_test_context();

        // Store transaction in mempool only
        let tx = create_test_transaction();
        let tx_hash = tx.tx_hash;
        store.put_transaction(&tx).unwrap();

        let p2p = Arc::new(Mutex::new(create_test_p2p_handle()));
        let service = TransactionServiceImpl::new(context, p2p);

        let request = Request::new(GetTransactionRequest {
            tx_hash: hex::encode(tx_hash),
        });

        let response = service.get_transaction_status(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.status, TransactionStatus::PendingMempool as i32);
        assert!(resp.block_hash.is_empty());
        assert_eq!(resp.confirmations, 0);
    }

    #[tokio::test]
    async fn get_transactions_by_address_empty() {
        let (context, _store, _writer) = create_test_context();
        let p2p = Arc::new(Mutex::new(create_test_p2p_handle()));
        let service = TransactionServiceImpl::new(context, p2p);

        let request = Request::new(GetTransactionsByAddressRequest {
            address: "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            role: AddressRole::Unspecified as i32,
            from_height: 0,
            to_height: 0,
            limit: 100,
            cursor: String::new(),
        });

        let response = service.get_transactions_by_address(request).await.unwrap();
        let resp = response.into_inner();

        assert!(resp.transactions.is_empty());
        assert!(!resp.has_more);
    }

    #[tokio::test]
    async fn get_transactions_by_address_sender() {
        let (context, store, _writer) = create_test_context();

        // Create a transaction with known sender
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let sender = Address::from_public_key(&pk);
        let recipient = Address::from_bytes([7u8; 32]);
        let tx = Arc::new(Transaction::new_transfer(
            sender, recipient, 100, 0, 10, &sk,
        ));

        let block = create_test_block(1, 1, [0u8; 32], vec![tx.clone()]);
        store.put_finalized_block(&block).unwrap();

        let p2p = Arc::new(Mutex::new(create_test_p2p_handle()));
        let service = TransactionServiceImpl::new(context, p2p);

        let request = Request::new(GetTransactionsByAddressRequest {
            address: hex::encode(sender.as_bytes()),
            role: AddressRole::Sender as i32,
            from_height: 0,
            to_height: 0,
            limit: 100,
            cursor: String::new(),
        });

        let response = service.get_transactions_by_address(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.transactions.len(), 1);
        assert!(!resp.has_more);
    }

    #[tokio::test]
    async fn get_transactions_by_address_recipient() {
        let (context, store, _writer) = create_test_context();

        // Create a transaction with known recipient
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let sender = Address::from_public_key(&pk);
        let recipient = Address::from_bytes([7u8; 32]);
        let tx = Arc::new(Transaction::new_transfer(
            sender, recipient, 100, 0, 10, &sk,
        ));

        let block = create_test_block(1, 1, [0u8; 32], vec![tx.clone()]);
        store.put_finalized_block(&block).unwrap();

        let p2p = Arc::new(Mutex::new(create_test_p2p_handle()));
        let service = TransactionServiceImpl::new(context, p2p);

        let request = Request::new(GetTransactionsByAddressRequest {
            address: hex::encode(recipient.as_bytes()),
            role: AddressRole::Recipient as i32,
            from_height: 0,
            to_height: 0,
            limit: 100,
            cursor: String::new(),
        });

        let response = service.get_transactions_by_address(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.transactions.len(), 1);
    }

    #[tokio::test]
    async fn get_transactions_by_address_with_limit() {
        let (context, store, _writer) = create_test_context();

        // Create multiple transactions
        let sk = TxSecretKey::generate(&mut rand::rngs::OsRng);
        let pk = sk.public_key();
        let sender = Address::from_public_key(&pk);

        let mut txs = Vec::new();
        for i in 0..5 {
            let recipient = Address::from_bytes([i as u8; 32]);
            let tx = Arc::new(Transaction::new_transfer(
                sender, recipient, 100, i, 10, &sk,
            ));
            txs.push(tx);
        }

        let block = create_test_block(1, 1, [0u8; 32], txs);
        store.put_finalized_block(&block).unwrap();

        let p2p = Arc::new(Mutex::new(create_test_p2p_handle()));
        let service = TransactionServiceImpl::new(context, p2p);

        let request = Request::new(GetTransactionsByAddressRequest {
            address: hex::encode(sender.as_bytes()),
            role: AddressRole::Sender as i32,
            from_height: 0,
            to_height: 0,
            limit: 2, // Only get 2
            cursor: String::new(),
        });

        let response = service.get_transactions_by_address(request).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.transactions.len(), 2);
        assert!(resp.has_more);
        assert!(!resp.next_cursor.is_empty());
    }

    #[tokio::test]
    async fn get_transactions_by_address_invalid_address() {
        let (context, _store, _writer) = create_test_context();
        let p2p = Arc::new(Mutex::new(create_test_p2p_handle()));
        let service = TransactionServiceImpl::new(context, p2p);

        let request = Request::new(GetTransactionsByAddressRequest {
            address: "invalid_address".to_string(),
            role: AddressRole::Unspecified as i32,
            from_height: 0,
            to_height: 0,
            limit: 100,
            cursor: String::new(),
        });

        let result = service.get_transactions_by_address(request).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }
}
