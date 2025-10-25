use std::{path::Path, time::Duration};

use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml, Yaml},
};
use serde::{Deserialize, Serialize};
use validator::Validate;

use crate::view_manager::leader_manager::LeaderSelectionStrategy;

/// [`ConsensusConfig`] sets the configuration values for the consensus protocol.
///
/// It contains the number of replicas in the consensus protocol, the number of faulty replicas,
/// and the leader selection strategy.
#[derive(Debug, Clone, Deserialize, Serialize, Validate)]
pub struct ConsensusConfig {
    /// The total number of replicas in the consensus protocol.
    #[validate(range(min = 6))]
    pub n: usize,
    /// The maximum number of faulty replicas in the consensus protocol.
    #[validate(range(min = 1))]
    pub f: usize,
    /// The maximum timeout duration allowed before a replica proposes a
    /// [`Nullify`] message to the network.
    pub view_timeout: Duration,
    /// The leader selection strategy to use.
    pub leader_manager: LeaderSelectionStrategy,
    /// The network in which the replica runs the consensus protocol.
    pub network: Network,
    /// The set of the (initial) peers in the consensus protocol.
    pub peers: Vec<String>,
}

impl ConsensusConfig {
    pub fn new(
        n: usize,
        f: usize,
        view_timeout: Duration,
        leader_manager: LeaderSelectionStrategy,
        network: Network,
        peers: Vec<String>,
    ) -> Self {
        Self {
            n,
            f,
            view_timeout,
            leader_manager,
            network,
            peers,
        }
    }

    /// [`from_path`] creates a [`ConsensusConfig`] from a .toml file
    /// or from environment variables.
    ///
    /// The configuration file is expected to be in the following format:
    /// ```toml
    /// [consensus]
    /// n = 10
    /// f = 3
    /// leader_manager = "RoundRobin"
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
        } else {
            return Err(anyhow::anyhow!(
                "Config file must have an extension (.toml, .yaml, or .yml)"
            ));
        }

        // Merge with environment variables (GATEWAY_ prefix)
        // Environment variables take precedence over file config
        figment = figment.merge(Env::prefixed("GATEWAY_").split("_"));

        let config: ConsensusConfig = figment
            .extract_inner("view_manager")
            .map_err(anyhow::Error::msg)?;

        config.validate()?;

        Ok(config)
    }
}

/// [`Network`] represents the network in which the replica runs the consensus protocol.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// [`Local`] represents a local network, where the replicas are running on the same machine.
    Local,

    /// [`Devnet`] represents a development network, where the replicas are running on different machines.
    Devnet,

    /// [`Testnet`] represents a test network, where the replicas are running on different machines.
    Testnet,

    /// [`Mainnet`] represents a main network, where the replicas are running on different machines.
    Mainnet,
}
