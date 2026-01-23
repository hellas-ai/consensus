//! Node crate - High-level validator node orchestration.
//!
//! This crate provides the `ValidatorNode` struct that manages all the services
//! required to run a validator in the Hellas consensus network:
//!
//! - **Storage**: Persistent block and state storage
//! - **Mempool**: Transaction pool management
//! - **Consensus**: BFT consensus protocol (Minimmit)
//! - **P2P**: Peer-to-peer networking
//! - **gRPC**: External API server
//!
//! ## Quick Start (Recommended)
//!
//! Load configuration from file and spawn a node:
//!
//! ```ignore
//! use node::{NodeConfig, ValidatorNode};
//! use p2p::ValidatorIdentity;
//!
//! // Load unified config from TOML/YAML
//! let config = NodeConfig::from_path("config.toml")?;
//! let identity = ValidatorIdentity::generate();
//! let logger = create_logger();
//!
//! // Spawn all services from config
//! let node = ValidatorNode::<6, 1, 3>::from_config(config, identity, logger)?;
//!
//! // Wait for P2P bootstrap
//! node.wait_ready().await;
//!
//! // Node is now running...
//!
//! // Graceful shutdown
//! node.shutdown(Duration::from_secs(10))?;
//! ```
//!
//! ## Builder Pattern (Alternative)
//!
//! For programmatic configuration:
//!
//! ```ignore
//! let node = ValidatorNodeBuilder::<6, 1, 3>::new()
//!     .with_consensus_config(consensus_config)
//!     .with_p2p_config(p2p_config)
//!     .with_rpc_config(rpc_config)
//!     .with_identity(identity)
//!     .with_storage_path("/var/lib/hellas/data")
//!     .with_logger(logger)
//!     .build()?;
//! ```
//!
//! ## Architecture
//!
//! The `ValidatorNode` follows a dependency-based spawn order and reverse shutdown order
//! to ensure clean lifecycle management. See the [`node`] module documentation for details.

pub mod config;
pub mod node;

// Re-export main types at crate root
pub use config::{IdentityConfig, NodeConfig};
pub use node::{ValidatorNode, ValidatorNodeBuilder};
