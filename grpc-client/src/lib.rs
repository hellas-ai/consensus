//! gRPC API for the Minimmit blockchain node.
//!
//! This crate provides a gRPC server that exposes:
//! - Transaction submission and querying
//! - Account state queries
//! - Block queries
//! - Node health and status
//! - Real-time subscriptions

pub mod config;
pub mod error;
pub mod server;
pub mod services;

/// Generated protobuf code from tonic-build.
/// This module is populated by the build.rs script.
#[allow(clippy::all)]
#[allow(clippy::pedantic)]
pub mod proto {
    tonic::include_proto!("minimmit.v1");
}

// Re-export key types for convenience
pub use config::{Network, RpcConfig};
pub use error::RpcError;
pub use server::RpcServer;
