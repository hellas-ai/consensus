//! P2P layer error types.

use thiserror::Error;

/// Errors that can occur in the P2P layer.
#[derive(Debug, Error)]
pub enum P2PError {
    /// Network connection error.
    #[error("Connection error: {0}")]
    Connection(String),

    /// Message serialization/deserialization error.
    #[error("Codec error: {0}")]
    Codec(#[from] rkyv::rancor::Error),

    /// Anyhow error.
    #[error("Anyhow error: {0}")]
    Anyhow(#[from] anyhow::Error),

    /// Wrong message type received.
    #[error("Unexpected message type: {0}")]
    MessageType(String),

    /// Peer not found in registry.
    #[error("Peer not found: {0}")]
    PeerNotFound(String),

    /// Channel send error.
    #[error("Send error: {0}")]
    SendError(String),

    /// Channel receive error.
    #[error("Receive error: {0}")]
    ReceiveError(String),

    /// Configuration error.
    #[error("Configuration error: {0}")]
    Config(String),

    /// Network not ready.
    #[error("Network not ready")]
    NotReady,

    /// Queue is full.
    #[error("Queue full - transaction broadcast queue at capacity")]
    QueueFull,
}
