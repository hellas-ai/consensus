//! SDK error types

use thiserror::Error;

/// SDK error type.
#[derive(Error, Debug)]
pub enum Error {
    /// Failed to connect to the node.
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    /// Request timed out.
    #[error("Request timeout")]
    Timeout,

    /// Transaction was rejected by the node.
    #[error("Transaction rejected: {0}")]
    TxRejected(String),

    /// Resource not found.
    #[error("Not found: {0}")]
    NotFound(String),

    /// Invalid argument provided.
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    /// Signing operation failed.
    #[error("Signing failed: {0}")]
    SigningFailed(String),

    /// gRPC transport error.
    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::Status),

    /// Parse error (hex, address, etc).
    #[error("Parse error: {0}")]
    Parse(String),

    /// Serialization error.
    #[error("Serialization error: {0}")]
    Serialization(String),
}

/// Result type alias for SDK operations.
pub type Result<T> = std::result::Result<T, Error>;
