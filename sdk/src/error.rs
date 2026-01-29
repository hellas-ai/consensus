//! SDK error types

use thiserror::Error;

/// SDK error type.
#[derive(Error, Debug)]
pub enum Error {
    /// Failed to connect to the node.
    #[error("Connection failed: {0}")]
    ConnectionFailed(#[source] tonic::transport::Error),

    /// Invalid endpoint URI.
    #[error("Invalid endpoint: {0}")]
    InvalidEndpoint(#[from] tonic::codegen::http::uri::InvalidUri),

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

    /// Hex decode error.
    #[error("Hex decode error: {0}")]
    HexDecode(#[from] hex::FromHexError),

    /// Serialization error.
    #[error("Serialization error: {0}")]
    Serialization(#[source] rkyv::rancor::Error),

    /// Max retries exceeded.
    #[error("Max retries exceeded after {num_retries} attempts")]
    MaxRetriesExceeded { num_retries: u32 },
}

/// Result type alias for SDK operations.
pub type Result<T> = std::result::Result<T, Error>;
