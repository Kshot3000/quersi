use std::path::{Path, PathBuf};

use axum::http::HeaderValue;
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub logging: LoggingConfig,
    pub remote_configs: RemoteConfigsConfig,
    pub exchange_rate: ExchangeRateConfig,
    pub risk_checker: RiskCheckerConfig,
    pub geo: GeoSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub cors_allowed_origins: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfigsConfig {
    pub wallet_configs_file: String,
}

/// Where the NEAR Intents geo gate reads its blocked countries and ranges.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeoSection {
    pub config_file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeRateConfig {
    pub api_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskCheckerConfig {
    pub etherscan_api_key: String,
    pub etherscan_base_url: String,
    pub infura_api_key: String,
    pub infura_base_url: String,
    pub etherscan_calls_per_sec: u32,
    pub max_concurrent_requests: usize,
}

impl Config {
    pub fn load(config_path: &str) -> Result<Self, ConfigError> {
        let settings = config::Config::builder()
            .add_source(config::File::new(config_path, config::FileFormat::Toml))
            .add_source(config::Environment::with_prefix("REMOTE_CONFIG").separator("__"))
            .build()?;

        let mut config: Self = settings.try_deserialize()?;
        config.resolve_relative_paths(config_path)?;
        Ok(config)
    }

    pub fn get_server_address(&self) -> String {
        format!("{}:{}", self.server.host, self.server.port)
    }

    pub fn get_cors_allowed_origins(&self) -> Vec<HeaderValue> {
        self.server
            .cors_allowed_origins
            .iter()
            .filter_map(|origin| match origin.parse() {
                Ok(value) => Some(value),
                Err(err) => {
                    tracing::warn!("Skipping invalid CORS origin {:?}: {}", origin, err);
                    None
                }
            })
            .collect()
    }

    pub fn wallet_configs_path(&self) -> PathBuf {
        PathBuf::from(&self.remote_configs.wallet_configs_file)
    }

    pub fn geo_config_path(&self) -> PathBuf {
        PathBuf::from(&self.geo.config_file)
    }

    fn resolve_relative_paths(&mut self, config_path: &str) -> Result<(), ConfigError> {
        let base_dir = Path::new(config_path)
            .parent()
            .ok_or(ConfigError::MissingConfigParent)?;

        self.remote_configs.wallet_configs_file =
            resolve_path(base_dir, &self.remote_configs.wallet_configs_file);
        self.geo.config_file = resolve_path(base_dir, &self.geo.config_file);

        Ok(())
    }
}

fn resolve_path(base_dir: &Path, path: &str) -> String {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        path.to_string()
    } else {
        base_dir.join(candidate).to_string_lossy().to_string()
    }
}
