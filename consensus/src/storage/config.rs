use std::path::Path;

use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml, Yaml},
};
use serde::{Deserialize, Serialize};
use validator::{Validate, ValidationError};

/// [`StorageConfig`] sets the configuration values for
/// persistent storage for each individual peer
#[derive(Debug, Clone, Deserialize, Serialize, Validate)]
pub struct StorageConfig {
    #[validate(custom(function = "validate_path"))]
    pub path: String,
}

impl StorageConfig {
    pub fn new(path: String) -> Self {
        Self { path }
    }

    /// [`from_path`] creates a [`StorageConfig`] from a .toml file
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

        let config: StorageConfig = figment
            .extract_inner("storage")
            .map_err(anyhow::Error::msg)?;

        config.validate()?;

        Ok(config)
    }
}

/// Validates that the path is not empty and points to a valid directory
fn validate_path(path: &str) -> Result<(), ValidationError> {
    if path.trim().is_empty() {
        return Err(ValidationError::new("path cannot be empty"));
    }

    let path_obj = Path::new(path);

    // Check if the path exists and is a directory
    if !path_obj.exists() {
        return Err(ValidationError::new("path does not exist"));
    }

    if !path_obj.is_dir() {
        return Err(ValidationError::new("path must be a directory"));
    }

    Ok(())
}
