//! P2P configuration types.

use consensus::crypto::aggregated::PeerId;
use figment::Figment;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// Unique namespace to avoid message replay attacks across applications.
pub const APPLICATION_NAMESPACE: &[u8] = b"_HELLAS_VALIDATOR_P2P";

/// P2P layer configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct P2PConfig {
    /// Local address to bind for listening.
    pub listen_addr: SocketAddr,

    /// External address that peers should use to connect to us.
    /// May differ from listen_addr if behind NAT.
    pub external_addr: SocketAddr,

    /// List of known validators (bootstrap peers).
    pub validators: Vec<ValidatorPeerInfo>,

    /// Total number of peers in the consensus network.
    pub total_number_peers: usize,

    /// Maximum number of faulty peers in the consensus network.
    pub maximum_number_faulty_peers: usize,

    /// Cluster ID for network namespace (replay protection).
    #[serde(default = "default_cluster_id")]
    pub cluster_id: String,

    /// Maximum message size in bytes.
    #[serde(default = "default_max_message_size")]
    pub max_message_size: u32,

    /// Maximum number of pending messages per channel.
    #[serde(default = "default_message_backlog")]
    pub message_backlog: usize,

    /// Rate limit for consensus messages (per second).
    #[serde(default = "default_consensus_rate")]
    pub consensus_rate_per_second: u32,

    /// Rate limit for transaction messages (per second).
    #[serde(default = "default_tx_rate")]
    pub tx_rate_per_second: u32,

    /// Timeout for bootstrap phase in milliseconds.
    /// If exceeded, signal ready anyway (for single-node testing).
    #[serde(default = "default_bootstrap_timeout_ms")]
    pub bootstrap_timeout_ms: u64,

    /// Interval between Ping retries during bootstrap (milliseconds).
    #[serde(default = "default_ping_interval_ms")]
    pub ping_interval_ms: u64,

    /// Size of the transaction broadcast queue.
    #[serde(default = "default_tx_broadcast_queue_size")]
    pub tx_broadcast_queue_size: usize,

    /// Whether this node is running in RPC-only mode.
    /// RPC nodes don't participate in consensus but receive finalized blocks.
    #[serde(default)]
    pub rpc_mode: bool,

    /// Maximum number of RPC node connections allowed (validators only).
    /// Has no effect when `rpc_mode` is true.
    #[serde(default = "default_max_rpc_connections")]
    pub max_rpc_connections: usize,
}

fn default_max_message_size() -> u32 {
    1024 * 1024 // 1 MB
}

fn default_cluster_id() -> String {
    "hellas".to_string()
}

fn default_message_backlog() -> usize {
    1024
}

fn default_consensus_rate() -> u32 {
    10000
}

fn default_tx_rate() -> u32 {
    50000
}

fn default_bootstrap_timeout_ms() -> u64 {
    30_000
}

fn default_ping_interval_ms() -> u64 {
    500
}

fn default_tx_broadcast_queue_size() -> usize {
    100_000
}

fn default_max_rpc_connections() -> usize {
    100
}

impl Default for P2PConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:9000".parse().unwrap(),
            external_addr: "0.0.0.0:9000".parse().unwrap(),
            validators: vec![],
            total_number_peers: 6, // 5 * f + 1
            maximum_number_faulty_peers: 1,
            cluster_id: default_cluster_id(),
            max_message_size: default_max_message_size(),
            message_backlog: default_message_backlog(),
            consensus_rate_per_second: default_consensus_rate(),
            tx_rate_per_second: default_tx_rate(),
            bootstrap_timeout_ms: default_bootstrap_timeout_ms(),
            ping_interval_ms: default_ping_interval_ms(),
            tx_broadcast_queue_size: default_tx_broadcast_queue_size(),
            rpc_mode: false,
            max_rpc_connections: default_max_rpc_connections(),
        }
    }
}

impl P2PConfig {
    /// Load configuration from a TOML file using figment.
    ///
    /// Falls back to environment variables with `HELLAS_VALIDATOR_P2P_` prefix.
    /// E.g., `P2P_LISTEN_ADDR=0.0.0.0:9001`
    ///
    /// # Example
    /// ```ignore
    /// let config = P2PConfig::from_file("config/p2p.toml")?;
    /// ```
    pub fn from_file<P: AsRef<std::path::Path>>(path: P) -> Result<Self, Box<figment::Error>> {
        use figment::providers::{Env, Format, Toml};
        Figment::new()
            .merge(Toml::file(path))
            .merge(Env::prefixed("HELLAS_VALIDATOR_P2P_"))
            .extract()
            .map_err(Box::new)
    }

    /// Load configuration with defaults, then merge from file, then env.
    ///
    /// Priority (highest to lowest): Env > TOML file > Defaults
    pub fn from_file_with_defaults<P: AsRef<std::path::Path>>(
        path: P,
    ) -> Result<Self, Box<figment::Error>> {
        use figment::providers::{Env, Format, Serialized, Toml};
        Figment::new()
            .merge(Serialized::defaults(Self::default()))
            .merge(Toml::file(path))
            .merge(Env::prefixed("P2P_"))
            .extract()
            .map_err(Box::new)
    }
}

/// Serde module for serializing as string, but deserializing from string OR integer.
/// This handles u64 values that exceed i64::MAX in TOML files.
mod string_or_int {
    use serde::{Deserialize, Deserializer, Serializer, de};
    use std::fmt::Display;
    use std::str::FromStr;

    pub fn serialize<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
    where
        T: Display,
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, T, D>(deserializer: D) -> Result<T, D::Error>
    where
        T: FromStr + TryFrom<u64> + TryFrom<i64>,
        T::Err: Display,
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum StringOrInt {
            String(String),
            Int(i64),
            UInt(u64),
        }

        match StringOrInt::deserialize(deserializer)? {
            StringOrInt::String(s) => T::from_str(&s).map_err(de::Error::custom),
            StringOrInt::Int(i) => {
                T::try_from(i as u64).map_err(|_| de::Error::custom("integer out of range"))
            }
            StringOrInt::UInt(u) => {
                T::try_from(u).map_err(|_| de::Error::custom("integer out of range"))
            }
        }
    }
}

/// Information about a validator peer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidatorPeerInfo {
    /// ED25519 public key (hex-encoded for config files).
    pub ed25519_public_key: String,

    /// BLS peer ID for consensus.
    #[serde(with = "string_or_int")]
    pub bls_peer_id: PeerId,

    /// Direct socket address (if known).
    pub address: Option<SocketAddr>,
}

impl ValidatorPeerInfo {
    /// Parse the ED25519 public key bytes from hex.
    pub fn parse_public_key_bytes(&self) -> Option<[u8; 32]> {
        let bytes = hex::decode(&self.ed25519_public_key).ok()?;
        if bytes.len() != 32 {
            return None;
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Some(arr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_default_config() {
        let config = P2PConfig::default();
        assert_eq!(
            config.listen_addr,
            "0.0.0.0:9000".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            config.external_addr,
            "0.0.0.0:9000".parse::<SocketAddr>().unwrap()
        );
        assert!(config.validators.is_empty());
        assert_eq!(config.total_number_peers, 6); // 5f + 1 for f=1
        assert_eq!(config.maximum_number_faulty_peers, 1);
        assert_eq!(config.max_message_size, 1024 * 1024);
        assert_eq!(config.message_backlog, 1024);
        assert_eq!(config.consensus_rate_per_second, 10000);
        assert_eq!(config.tx_rate_per_second, 50000);
        assert_eq!(config.bootstrap_timeout_ms, 30_000);
        assert_eq!(config.ping_interval_ms, 500);
    }

    #[test]
    fn test_from_toml_minimal() {
        let toml = r#"
listen_addr = "127.0.0.1:8000"
external_addr = "1.2.3.4:8000"
validators = []
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(toml.as_bytes()).unwrap();

        let config = P2PConfig::from_file_with_defaults(file.path()).unwrap();
        assert_eq!(
            config.listen_addr,
            "127.0.0.1:8000".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            config.external_addr,
            "1.2.3.4:8000".parse::<SocketAddr>().unwrap()
        );
        // Defaults should apply
        assert_eq!(config.max_message_size, 1024 * 1024);
    }

    #[test]
    fn test_from_toml_with_validators() {
        let toml = r#"
listen_addr = "0.0.0.0:9000"
external_addr = "0.0.0.0:9000"

[[validators]]
ed25519_public_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
bls_peer_id = 0
address = "192.168.1.1:9000"

[[validators]]
ed25519_public_key = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
bls_peer_id = 1
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(toml.as_bytes()).unwrap();

        let config = P2PConfig::from_file_with_defaults(file.path()).unwrap();
        assert_eq!(config.validators.len(), 2);
        assert_eq!(config.validators[0].bls_peer_id, 0);
        assert_eq!(
            config.validators[0].address,
            Some("192.168.1.1:9000".parse::<SocketAddr>().unwrap())
        );
        assert_eq!(config.validators[1].bls_peer_id, 1);
        assert!(config.validators[1].address.is_none());
    }

    #[test]
    fn test_from_toml_custom_values() {
        let toml = r#"
listen_addr = "0.0.0.0:9000"
external_addr = "0.0.0.0:9000"
validators = []
max_message_size = 2097152
message_backlog = 2048
consensus_rate_per_second = 20000
tx_rate_per_second = 100000
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(toml.as_bytes()).unwrap();

        let config = P2PConfig::from_file_with_defaults(file.path()).unwrap();
        assert_eq!(config.max_message_size, 2097152);
        assert_eq!(config.message_backlog, 2048);
        assert_eq!(config.consensus_rate_per_second, 20000);
        assert_eq!(config.tx_rate_per_second, 100000);
    }

    #[test]
    fn test_validator_parse_public_key_valid() {
        let validator = ValidatorPeerInfo {
            ed25519_public_key: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            bls_peer_id: 0,
            address: None,
        };
        let pk = validator.parse_public_key_bytes().unwrap();
        assert_eq!(pk[0], 0x01);
        assert_eq!(pk[1], 0x23);
    }

    #[test]
    fn test_validator_parse_public_key_invalid_length() {
        let validator = ValidatorPeerInfo {
            ed25519_public_key: "0123456789abcdef".to_string(), // Too short
            bls_peer_id: 0,
            address: None,
        };
        assert!(validator.parse_public_key_bytes().is_none());
    }

    #[test]
    fn test_validator_parse_public_key_invalid_hex() {
        let validator = ValidatorPeerInfo {
            ed25519_public_key: "not_valid_hex".to_string(),
            bls_peer_id: 0,
            address: None,
        };
        assert!(validator.parse_public_key_bytes().is_none());
    }

    #[test]
    fn test_missing_file_error() {
        let result = P2PConfig::from_file("/nonexistent/path/config.toml");
        // from_file without defaults should fail on missing file
        // But figment with Toml::file makes it optional, so this may succeed with empty
        // Let's test from_file explicitly requires fields
        assert!(result.is_err());
    }
}
