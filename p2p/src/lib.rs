//! P2P networking layer using Commonware primitives.
//!
//! This crate provides blockchain-optimized peer-to-peer networking
//! with deterministic simulation support for testing.

pub mod config;
pub mod error;
pub mod identity;
pub mod message;
pub mod network;
pub mod peer;
pub mod protocols;
pub mod service;

pub use config::P2PConfig;
pub use error::P2PError;
pub use identity::ValidatorIdentity;
pub use network::NetworkService;
pub use service::{P2PHandle, route_incoming_message, spawn};
