//! gRPC server configuration.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};

/// Network environment for the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// Production mainnet.
    Mainnet,
    /// Public testnet.
    Testnet,
    /// Local development network.
    #[default]
    Local,
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Network::Mainnet => write!(f, "mainnet"),
            Network::Testnet => write!(f, "testnet"),
            Network::Local => write!(f, "local"),
        }
    }
}

/// Configuration for the RPC server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcConfig {
    /// Address to listen on (e.g., "0.0.0.0:50051")
    #[serde(with = "socket_addr_serde")]
    pub listen_addr: SocketAddr,
    /// Maximum concurrent streams per connection
    pub max_concurrent_streams: u32,
    /// Request timeout in seconds
    pub request_timeout_secs: u64,
    /// This node's peer ID
    pub peer_id: u64,
    /// Network environment (mainnet, testnet, local)
    pub network: Network,
    /// Total validators in network
    pub total_validators: u32,
    /// Fault tolerance parameter F
    pub f: u32,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:50051".parse().unwrap(),
            max_concurrent_streams: 100,
            request_timeout_secs: 30,
            peer_id: 0,
            network: Network::Local,
            total_validators: 4,
            f: 1,
        }
    }
}

impl RpcConfig {
    /// Load configuration from a file path.
    ///
    /// Supports TOML and YAML formats. Environment variables with the `GRPC_`
    /// prefix will override file values.
    ///
    /// # Example config (TOML)
    /// ```toml
    /// [grpc]
    /// listen_addr = "0.0.0.0:50051"
    /// max_concurrent_streams = 100
    /// request_timeout_secs = 30
    /// peer_id = 0
    /// network = "local"
    /// total_validators = 4
    /// f = 1
    /// ```
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();

        let mut figment = Figment::new();

        // Detect file format based on extension
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            figment = match ext {
                "toml" => figment.merge(Toml::file(path)),
                _ => {
                    return Err(anyhow::anyhow!(
                        "Unsupported config file format: {}. Use .toml",
                        ext
                    ));
                }
            };
        } else {
            return Err(anyhow::anyhow!(
                "Config file must have an extension (.toml, .yaml, or .yml)"
            ));
        }

        // Merge with environment variables (GRPC_ prefix)
        // Environment variables take precedence over file config
        figment = figment.merge(Env::prefixed("GRPC_").split("_"));

        let config: RpcConfig = figment.extract_inner("grpc").map_err(anyhow::Error::msg)?;

        Ok(config)
    }
}

/// Custom serde module for SocketAddr to handle string serialization.
mod socket_addr_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::net::SocketAddr;

    pub fn serialize<S>(addr: &SocketAddr, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        addr.to_string().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SocketAddr, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_display() {
        assert_eq!(Network::Mainnet.to_string(), "mainnet");
        assert_eq!(Network::Testnet.to_string(), "testnet");
        assert_eq!(Network::Local.to_string(), "local");
    }

    #[test]
    fn network_default_is_local() {
        assert_eq!(Network::default(), Network::Local);
    }

    #[test]
    fn network_serde_roundtrip() {
        let networks = [Network::Mainnet, Network::Testnet, Network::Local];
        for network in networks {
            let json = serde_json::to_string(&network).unwrap();
            let parsed: Network = serde_json::from_str(&json).unwrap();
            assert_eq!(network, parsed);
        }
    }

    #[test]
    fn network_deserialize_from_string() {
        let mainnet: Network = serde_json::from_str("\"mainnet\"").unwrap();
        assert_eq!(mainnet, Network::Mainnet);

        let testnet: Network = serde_json::from_str("\"testnet\"").unwrap();
        assert_eq!(testnet, Network::Testnet);

        let local: Network = serde_json::from_str("\"local\"").unwrap();
        assert_eq!(local, Network::Local);
    }

    #[test]
    fn config_default_values() {
        let config = RpcConfig::default();
        assert_eq!(config.listen_addr.to_string(), "0.0.0.0:50051");
        assert_eq!(config.max_concurrent_streams, 100);
        assert_eq!(config.request_timeout_secs, 30);
        assert_eq!(config.peer_id, 0);
        assert_eq!(config.network, Network::Local);
        assert_eq!(config.total_validators, 4);
        assert_eq!(config.f, 1);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = RpcConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: RpcConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config.listen_addr, parsed.listen_addr);
        assert_eq!(config.network, parsed.network);
        assert_eq!(config.peer_id, parsed.peer_id);
    }

    #[test]
    fn from_path_unsupported_extension() {
        let result = RpcConfig::from_path("config.json");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unsupported"));
    }

    #[test]
    fn from_path_no_extension() {
        let result = RpcConfig::from_path("config");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("extension"));
    }
}
