//! Transaction service implementation.

use std::sync::Arc;

use crate::server::RpcContext;

/// Implementation of the TransactionService gRPC service.
pub struct TransactionServiceImpl {
    context: Arc<RpcContext>,
}

impl TransactionServiceImpl {
    /// Create a new TransactionService implementation.
    pub fn new(context: Arc<RpcContext>) -> Self {
        Self { context }
    }
}

// TODO: Implement tonic service trait once proto generation is complete
//
// #[tonic::async_trait]
// impl TransactionService for TransactionServiceImpl {
//     async fn submit_transaction(
//         &self,
//         request: Request<SubmitTransactionRequest>,
//     ) -> Result<Response<SubmitTransactionResponse>, Status> {
//         let req = request.into_inner();
//
//         // 1. Deserialize transaction from bytes
//         let tx = match deserialize_transaction(&req.transaction_bytes) {
//             Ok(tx) => tx,
//             Err(e) => {
//                 return Ok(Response::new(SubmitTransactionResponse {
//                     success: false,
//                     tx_hash: String::new(),
//                     error_message: e.to_string(),
//                     error_code: ErrorCode::InvalidFormat as i32,
//                 }));
//             }
//         };
//
//         // 2. Verify signature
//         if !tx.verify() {
//             return Ok(Response::new(SubmitTransactionResponse {
//                 success: false,
//                 tx_hash: String::new(),
//                 error_message: "Invalid signature".to_string(),
//                 error_code: ErrorCode::InvalidSignature as i32,
//             }));
//         }
//
//         let tx_hash = hex::encode(tx.tx_hash);
//
//         // 3. Send to mempool
//         // self.context.mempool_producer.push(tx.clone())...
//
//         // 4. Broadcast via P2P
//         // self.context.p2p_handle.broadcast_transaction(tx)...
//
//         Ok(Response::new(SubmitTransactionResponse {
//             success: true,
//             tx_hash,
//             error_message: String::new(),
//             error_code: ErrorCode::Unspecified as i32,
//         }))
//     }
// }
