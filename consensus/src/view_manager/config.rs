use std::{path::Path, time::Duration};

use anyhow::Result;
use config::{Config, Environment, File};
use serde::{Deserialize, Serialize};

use crate::view_manager::leader_manager::LeaderSelectionStrategy;

/// [`ConsensusConfig`] sets the configuration values for the consensus protocol.
///
/// It contains the number of replicas in the consensus protocol, the number of faulty replicas,
/// and the leader selection strategy.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConsensusConfig {
    /// The total number of replicas in the consensus protocol.
    pub n: usize,
    /// The maximum number of faulty replicas in the consensus protocol.
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
        let config = Config::builder()
            .add_source(File::with_name(path.as_ref().to_str().unwrap()))
            .add_source(
                Environment::with_prefix("consensus")
                    .keep_prefix(true)
                    .separator("__"),
            )
            .build()?;

        config.get::<Self>("consensus").map_err(anyhow::Error::msg)
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
