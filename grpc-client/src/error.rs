//! Error types for the gRPC API.

use thiserror::Error;
use tonic::Status;

/// RPC-specific errors that can occur during request processing.
#[derive(Debug, Error)]
pub enum RpcError {
    /// Transaction signature verification failed
    #[error("Invalid transaction signature")]
    InvalidSignature,

    /// Transaction format is invalid (deserialization failed)
    #[error("Invalid transaction format: {0}")]
    InvalidFormat(String),

    /// Mempool is full, cannot accept more transactions
    #[error("Mempool is full")]
    MempoolFull,

    /// Transaction already exists in mempool or chain
    #[error("Duplicate transaction")]
    DuplicateTransaction,

    /// Sender has insufficient balance
    #[error("Insufficient balance")]
    InsufficientBalance,

    /// Transaction nonce is invalid
    #[error("Invalid nonce: expected {expected}, got {got}")]
    InvalidNonce { expected: u64, got: u64 },

    /// Failed to broadcast transaction via P2P
    #[error("Broadcast failed: {0}")]
    BroadcastFailed(String),

    /// Requested resource was not found
    #[error("Not found: {0}")]
    NotFound(String),

    /// Invalid request parameters
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    /// Internal server error
    #[error("Internal error: {0}")]
    Internal(String),

    /// Storage/database error
    #[error("Storage error: {0}")]
    Storage(#[from] anyhow::Error),
}

impl From<RpcError> for Status {
    fn from(err: RpcError) -> Self {
        match err {
            RpcError::InvalidSignature => Status::invalid_argument(err.to_string()),
            RpcError::InvalidFormat(msg) => {
                Status::invalid_argument(format!("Invalid format: {}", msg))
            }
            RpcError::MempoolFull => Status::resource_exhausted(err.to_string()),
            RpcError::DuplicateTransaction => Status::already_exists(err.to_string()),
            RpcError::InsufficientBalance => Status::failed_precondition(err.to_string()),
            RpcError::InvalidNonce { expected, got } => Status::failed_precondition(format!(
                "Invalid nonce: expected {}, got {}",
                expected, got
            )),
            RpcError::BroadcastFailed(msg) => {
                Status::unavailable(format!("Broadcast failed: {}", msg))
            }
            RpcError::NotFound(msg) => Status::not_found(msg),
            RpcError::InvalidArgument(msg) => Status::invalid_argument(msg),
            RpcError::Internal(msg) => Status::internal(msg),
            RpcError::Storage(err) => Status::internal(format!("Storage error: {}", err)),
        }
    }
}

/// Result type for RPC operations
pub type RpcResult<T> = Result<T, RpcError>;
