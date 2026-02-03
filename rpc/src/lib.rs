//! RPC Node crate for Hellas blockchain.
//!
//! Provides a lightweight RPC node that:
//! - Connects to validators via P2P
//! - Syncs finalized blocks with L-notarization proofs
//! - Serves read-only gRPC queries
//!
//! RPC nodes do NOT participate in consensus.

pub mod config;
pub mod grpc;
pub mod identity;
pub mod node;
pub mod p2p;
pub mod sync;

pub use config::RpcConfig;
pub use grpc::{GrpcServerConfig, RpcGrpcServer};
pub use identity::RpcIdentity;
pub use node::RpcNode;
pub use p2p::{BlockRequestCommand, RpcP2PHandle, spawn_rpc_p2p};
pub use sync::{BlockSyncer, SyncConfig, SyncState};
