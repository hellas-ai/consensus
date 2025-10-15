use std::path::Path;

use anyhow::Result;
use config::{Config, Environment, File};
use serde::{Deserialize, Serialize};

/// [`StorageConfig`] sets the configuration values for
/// persistent storage for each individual peer
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageConfig {
    pub path: String,
}

impl StorageConfig {
    pub fn new(path: String) -> Self {
        Self { path }
    }

    /// [`from_path`] creates a [`StorageConfig`] from a .toml file
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let config = Config::builder()
            .add_source(File::with_name(path.as_ref().to_str().unwrap()))
            .add_source(
                Environment::with_prefix("STORAGE")
                    .keep_prefix(true)
                    .separator("__"),
            )
            .build()?;

        config.get::<Self>("storage").map_err(anyhow::Error::msg)
    }
}
