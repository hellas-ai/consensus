//! RPC node configuration.

use std::net::SocketAddr;
use std::path::PathBuf;

use figment::Figment;
use figment::providers::{Env, Format, Toml};
use p2p::config::ValidatorPeerInfo;
use serde::{Deserialize, Serialize};

/// Configuration for an RPC node.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcConfig {
    /// Validators to connect to for block sync.
    #[serde(default)]
    pub validators: Vec<ValidatorPeerInfo>,

    /// gRPC server bind address.
    #[serde(default = "default_grpc_addr")]
    pub grpc_addr: SocketAddr,

    /// P2P listen address.
    #[serde(default = "default_p2p_addr")]
    pub p2p_addr: SocketAddr,

    /// Data directory for storage.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    /// Cluster identifier (must match validators).
    #[serde(default = "default_cluster_id")]
    pub cluster_id: String,
}

fn default_grpc_addr() -> SocketAddr {
    "0.0.0.0:50051".parse().unwrap()
}

fn default_p2p_addr() -> SocketAddr {
    "0.0.0.0:9000".parse().unwrap()
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("./rpc-data")
}

fn default_cluster_id() -> String {
    "hellas-mainnet".to_string()
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            validators: vec![],
            grpc_addr: default_grpc_addr(),
            p2p_addr: default_p2p_addr(),
            data_dir: default_data_dir(),
            cluster_id: default_cluster_id(),
        }
    }
}

impl RpcConfig {
    /// Load configuration from a TOML file with env overrides.
    pub fn load(path: &str) -> Result<Self, Box<figment::Error>> {
        Figment::new()
            .merge(Toml::file(path))
            .merge(Env::prefixed("RPC_").split("_"))
            .extract()
            .map_err(Box::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::Serialized;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_default_config() {
        let config = RpcConfig::default();
        assert!(config.validators.is_empty());
        assert_eq!(config.grpc_addr.port(), 50051);
        assert_eq!(config.p2p_addr.port(), 9000);
        assert_eq!(config.cluster_id, "hellas-mainnet");
        assert_eq!(config.data_dir, PathBuf::from("./rpc-data"));
    }

    #[test]
    fn test_full_toml_config() {
        let toml_content = r#"
# RPC Node Configuration

# Cluster identifier (must match validators)
cluster_id = "hellas-testnet"

# gRPC server bind address
grpc_addr = "0.0.0.0:50052"

# P2P listen address
p2p_addr = "0.0.0.0:9001"

# Data directory for storage
data_dir = "/var/lib/rpc-node/data"

# List of validators to connect to
[[validators]]
ed25519_public_key = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"
bls_peer_id = "1234567890123456789"
address = "192.168.1.10:9000"

[[validators]]
ed25519_public_key = "b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3"
bls_peer_id = "2345678901234567890"
address = "192.168.1.11:9000"

[[validators]]
ed25519_public_key = "c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4"
bls_peer_id = "3456789012345678901"
# address is optional
"#;

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(toml_content.as_bytes()).unwrap();

        let config = RpcConfig::load(file.path().to_str().unwrap()).unwrap();

        assert_eq!(config.cluster_id, "hellas-testnet");
        assert_eq!(config.grpc_addr.port(), 50052);
        assert_eq!(config.p2p_addr.port(), 9001);
        assert_eq!(config.data_dir, PathBuf::from("/var/lib/rpc-node/data"));
        assert_eq!(config.validators.len(), 3);

        // Check first validator
        assert_eq!(
            config.validators[0].ed25519_public_key,
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"
        );
        assert_eq!(config.validators[0].bls_peer_id, 1234567890123456789);
        assert!(config.validators[0].address.is_some());

        // Check third validator (no address)
        assert!(config.validators[2].address.is_none());
    }

    #[test]
    fn test_partial_config_with_defaults() {
        // Only specify required fields, let defaults fill in the rest
        let toml_content = r#"
[[validators]]
ed25519_public_key = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
bls_peer_id = "999888777666555444"
"#;

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(toml_content.as_bytes()).unwrap();

        let config = RpcConfig::load(file.path().to_str().unwrap()).unwrap();

        // Validators should be parsed
        assert_eq!(config.validators.len(), 1);

        // Defaults should be applied
        assert_eq!(config.grpc_addr.port(), 50051); // default
        assert_eq!(config.p2p_addr.port(), 9000); // default
        assert_eq!(config.cluster_id, "hellas-mainnet"); // default
    }

    #[test]
    fn test_empty_config_uses_all_defaults() {
        let toml_content = "";

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(toml_content.as_bytes()).unwrap();

        let config = RpcConfig::load(file.path().to_str().unwrap()).unwrap();

        assert!(config.validators.is_empty());
        assert_eq!(config.grpc_addr, default_grpc_addr());
        assert_eq!(config.p2p_addr, default_p2p_addr());
        assert_eq!(config.data_dir, default_data_dir());
        assert_eq!(config.cluster_id, default_cluster_id());
    }

    #[test]
    fn test_config_serialization_roundtrip() {
        let original = RpcConfig {
            validators: vec![ValidatorPeerInfo {
                ed25519_public_key: "abcd1234".to_string(),
                bls_peer_id: 123456789,
                bls_public_key: None,
                address: Some("127.0.0.1:9000".parse().unwrap()),
            }],
            grpc_addr: "0.0.0.0:50053".parse().unwrap(),
            p2p_addr: "0.0.0.0:9002".parse().unwrap(),
            data_dir: PathBuf::from("/custom/path"),
            cluster_id: "my-cluster".to_string(),
        };

        // Use figment's Serialized provider for roundtrip
        let parsed: RpcConfig = Figment::new()
            .merge(Serialized::defaults(&original))
            .extract()
            .unwrap();

        assert_eq!(parsed.cluster_id, original.cluster_id);
        assert_eq!(parsed.grpc_addr, original.grpc_addr);
        assert_eq!(parsed.validators.len(), 1);
        assert_eq!(
            parsed.validators[0].bls_peer_id,
            original.validators[0].bls_peer_id
        );
    }

    #[test]
    fn test_bls_peer_id_as_string() {
        // BLS peer ID can be specified as string (for large values that overflow i64)
        let toml_content = r#"
[[validators]]
ed25519_public_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
bls_peer_id = "18446744073709551615"
"#;

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(toml_content.as_bytes()).unwrap();

        let config = RpcConfig::load(file.path().to_str().unwrap()).unwrap();
        assert_eq!(config.validators[0].bls_peer_id, u64::MAX);
    }

    #[test]
    fn test_figment_env_override() {
        // Test that environment variables can override config values
        // This uses figment's Serialized provider to simulate env vars
        let base = RpcConfig::default();

        let config: RpcConfig = Figment::new()
            .merge(Serialized::defaults(base))
            .merge(Serialized::global("grpc_addr", "0.0.0.0:60000"))
            .merge(Serialized::global("cluster_id", "env-override-cluster"))
            .extract()
            .unwrap();

        assert_eq!(config.grpc_addr.port(), 60000);
        assert_eq!(config.cluster_id, "env-override-cluster");
    }
}
