//! Node configuration types.
//!
//! Combines all service configurations into a unified `NodeConfig` that can be
//! loaded from TOML/YAML files or environment variables.

use std::path::Path;

use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml, Yaml},
};
use serde::{Deserialize, Serialize};

use consensus::consensus_manager::config::ConsensusConfig;
use consensus::storage::config::StorageConfig;
use grpc_client::config::RpcConfig;
use p2p::config::P2PConfig;

/// Validator identity configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentityConfig {
    /// Path to the BLS secret key file (hex-encoded).
    /// If not provided, a new key will be generated.
    pub bls_secret_key_path: Option<String>,

    /// Path to the Ed25519 secret key file for P2P identity.
    /// If not provided, a new key will be generated.
    pub ed25519_secret_key_path: Option<String>,
}

/// Complete node configuration combining all service configs.
///
/// # Example TOML
///
/// ```toml
/// [consensus]
/// n = 4
/// f = 1
/// view_timeout = { secs = 5, nanos = 0 }
/// leader_manager = "RoundRobin"
/// network = "local"
/// peers = ["peer1_pubkey", "peer2_pubkey", ...]
///
/// [storage]
/// path = "/var/lib/hellas/data"
///
/// [p2p]
/// listen_addr = "0.0.0.0:9000"
/// external_addr = "1.2.3.4:9000"
///
/// [rpc]
/// listen_addr = "0.0.0.0:50051"
///
/// [identity]
/// bls_secret_key_path = "/etc/hellas/bls.key"
/// ed25519_secret_key_path = "/etc/hellas/ed25519.key"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Consensus protocol configuration.
    pub consensus: ConsensusConfig,

    /// Persistent storage configuration.
    pub storage: StorageConfig,

    /// P2P networking configuration.
    pub p2p: P2PConfig,

    /// gRPC server configuration.
    pub rpc: RpcConfig,

    /// Validator identity configuration.
    #[serde(default)]
    pub identity: IdentityConfig,
}

impl NodeConfig {
    /// Load configuration from a file path.
    ///
    /// Supports TOML (.toml) and YAML (.yaml, .yml) formats.
    /// Environment variables can override file values using the `NODE_` prefix.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let config = NodeConfig::from_path("config.toml")?;
    /// ```
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();

        let mut figment = Figment::new();

        // Detect file format based on extension
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            figment = match ext {
                "toml" => figment.merge(Toml::file(path)),
                "yaml" | "yml" => figment.merge(Yaml::file(path)),
                _ => {
                    return Err(anyhow::anyhow!(
                        "Unsupported config file format: {}. Use .toml, .yaml, or .yml",
                        ext
                    ));
                }
            };
        }

        // Allow environment variable overrides with NODE_ prefix
        figment = figment.merge(Env::prefixed("NODE_").split("__"));

        let config: NodeConfig = figment.extract()?;
        Ok(config)
    }

    /// Load configuration from environment variables only.
    ///
    /// Uses the `NODE_` prefix for all variables.
    /// Nested fields use double underscore: `NODE_CONSENSUS__N=4`
    pub fn from_env() -> Result<Self> {
        let figment = Figment::new().merge(Env::prefixed("NODE_").split("__"));

        let config: NodeConfig = figment.extract()?;
        Ok(config)
    }
}
