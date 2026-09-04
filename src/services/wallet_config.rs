use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};

use serde_json::Value;
use tokio::{task::JoinHandle, time};

#[derive(Debug, thiserror::Error)]
pub enum WalletConfigsError {
    #[error("failed to read wallet configs file: {0}")]
    ReadFile(#[from] std::io::Error),
    #[error("failed to parse wallet configs JSON: {0}")]
    ParseJson(#[from] serde_json::Error),
    #[error("invalid wallet configs: {0}")]
    Invalid(String),
    #[error("failed to read wallet configs: {0}")]
    ReadLock(String),
}

#[derive(Debug)]
pub struct WalletConfigService {
    wallet_configs: Arc<RwLock<Value>>,
    _watch_task: JoinHandle<()>,
}

impl WalletConfigService {
    pub fn new(file_path: impl Into<PathBuf>) -> Result<Self, WalletConfigsError> {
        let file_path = file_path.into();

        let configs = Self::load_flags_sync(&file_path)?;
        let wallet_configs = Arc::new(RwLock::new(configs));

        let wallet_flags_clone = wallet_configs.clone();
        let poll_interval = Duration::from_millis(250);

        let watch_task = tokio::spawn(async move {
            let mut interval = time::interval(poll_interval);
            interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
            let mut last_reload_failed = false;

            loop {
                interval.tick().await;

                match Self::load_flags_async(&file_path).await {
                    Ok(updated_flags) => {
                        last_reload_failed = false;
                        if let Ok(mut write_guard) = wallet_flags_clone.write() {
                            if *write_guard == updated_flags {
                                continue;
                            }

                            *write_guard = updated_flags;
                            tracing::info!("Wallet configs reloaded from {}", file_path.display());
                        }
                    }
                    Err(err) => {
                        if !last_reload_failed {
                            tracing::warn!(
                                "Failed to reload wallet configs from {}: {}. Using last known good configs.",
                                file_path.display(),
                                err
                            );
                            last_reload_failed = true;
                        }
                    }
                }
            }
        });

        Ok(Self {
            wallet_configs,
            _watch_task: watch_task,
        })
    }

    pub fn get_wallet_configs(&self) -> Result<Value, WalletConfigsError> {
        let guard = self
            .wallet_configs
            .read()
            .map_err(|_| WalletConfigsError::ReadLock("lock poisoned".to_string()))?;
        Ok(guard.clone())
    }

    fn load_flags_sync(config_path: &Path) -> Result<Value, WalletConfigsError> {
        Self::parse(&std::fs::read(config_path)?)
    }

    async fn load_flags_async(config_path: &Path) -> Result<Value, WalletConfigsError> {
        Self::parse(&tokio::fs::read(config_path).await?)
    }

    fn parse(content: &[u8]) -> Result<Value, WalletConfigsError> {
        let flags = serde_json::from_slice::<Value>(content)?;
        validate_endpoints(&flags)?;
        Ok(flags)
    }
}

const RPC_SCHEMES: &[&str] = &["http", "https", "ws", "wss"];
const HTTP_SCHEMES: &[&str] = &["http", "https"];

/// Flags stay opaque, but a malformed `endpoints` block would strand every
/// wallet on an unreachable network, so its shape is checked before serving.
/// Keys are optional; a present key must be a URL (or a non-empty list of them).
fn validate_endpoints(configs: &Value) -> Result<(), WalletConfigsError> {
    let Some(endpoints) = configs.get("endpoints") else {
        return Ok(());
    };
    let endpoints = endpoints
        .as_object()
        .ok_or_else(|| WalletConfigsError::Invalid("endpoints must be an object".into()))?;

    for (key, schemes) in [("rpc", RPC_SCHEMES), ("graphQl", HTTP_SCHEMES)] {
        let Some(value) = endpoints.get(key) else {
            continue;
        };
        let urls = value
            .as_array()
            .filter(|urls| !urls.is_empty())
            .ok_or_else(|| {
                WalletConfigsError::Invalid(format!(
                    "endpoints.{key} must be a non-empty array of URLs, got {value}"
                ))
            })?;
        for url in urls {
            validate_url(key, url, schemes)?;
        }
    }
    for key in ["explorer", "senoti"] {
        if let Some(value) = endpoints.get(key) {
            validate_url(key, value, HTTP_SCHEMES)?;
        }
    }
    Ok(())
}

fn validate_url(key: &str, value: &Value, schemes: &[&str]) -> Result<(), WalletConfigsError> {
    let valid = value
        .as_str()
        .and_then(|raw| reqwest::Url::parse(raw).ok())
        .is_some_and(|url| schemes.contains(&url.scheme()) && url.host_str().is_some());
    if valid {
        Ok(())
    } else {
        Err(WalletConfigsError::Invalid(format!(
            "endpoints.{key} must hold {} URLs, got {value}",
            schemes.join("/")
        )))
    }
}

impl Drop for WalletConfigService {
    fn drop(&mut self) {
        self._watch_task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use uuid::Uuid;

    fn unique_temp_dir() -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!("remote-config-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_flags(dir: &Path, flags_json: &str) -> PathBuf {
        let config_path = dir.join("flags.json");
        std::fs::write(&config_path, flags_json).expect("write flags");
        config_path
    }

    const VALID_FLAGS: &str = r#"{
  "enableTestButtons": false,
  "enableKeystoneHardwareWallet": false,
  "enableHighSecurity": true,
  "enableRemoteNotifications": true,
  "enableSwap": true
}"#;

    const WITH_ENDPOINTS: &str = r#"{
  "enableSwap": true,
  "endpoints": {
    "rpc": ["wss://rpc-1.example.net", "https://rpc-2.example.net"],
    "graphQl": ["https://indexer.example.net/v1/graphql"],
    "explorer": "https://explorer.example.net",
    "senoti": "https://snt.example.net/api"
  }
}"#;

    const UPDATED_FLAGS: &str = r#"{
  "enableTestButtons": true,
  "enableKeystoneHardwareWallet": true,
  "enableHighSecurity": false,
  "enableRemoteNotifications": false,
  "enableSwap": false
}"#;

    #[tokio::test]
    async fn new_loads_initial_flags_from_file() {
        let dir = unique_temp_dir();
        let config_path = write_flags(&dir, VALID_FLAGS);

        let service = WalletConfigService::new(config_path).expect("service should initialize");

        let configs = service.get_wallet_configs().unwrap();
        assert!(!configs["enableTestButtons"].as_bool().unwrap());
        assert!(!configs["enableKeystoneHardwareWallet"].as_bool().unwrap());
        assert!(configs["enableHighSecurity"].as_bool().unwrap());
        assert!(configs["enableRemoteNotifications"].as_bool().unwrap());
        assert!(configs["enableSwap"].as_bool().unwrap());

        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn new_rejects_missing_file_at_startup() {
        let dir = unique_temp_dir();
        let config_path = dir.join("flags.json");

        let err =
            WalletConfigService::new(config_path).expect_err("missing config must fail startup");

        assert!(matches!(err, WalletConfigsError::ReadFile(_)));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn new_rejects_invalid_json_at_startup() {
        let dir = unique_temp_dir();
        let config_path = write_flags(&dir, "{ invalid json }");

        let err =
            WalletConfigService::new(config_path).expect_err("invalid JSON must fail startup");

        assert!(matches!(err, WalletConfigsError::ParseJson(_)));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn new_accepts_extra_json_fields_without_redeploy() {
        let dir = unique_temp_dir();
        let with_extra = r#"{
  "enableTestButtons": false,
  "enableKeystoneHardwareWallet": false,
  "enableHighSecurity": true,
  "enableRemoteNotifications": true,
  "enableSwap": true,
  "enableNewFeature": true
}"#;
        let config_path = write_flags(&dir, with_extra);

        let service =
            WalletConfigService::new(config_path).expect("extra fields should be allowed");

        let configs = service.get_wallet_configs().unwrap();
        assert!(configs["enableNewFeature"].as_bool().unwrap());

        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn new_accepts_endpoint_overrides() {
        let dir = unique_temp_dir();
        let config_path = write_flags(&dir, WITH_ENDPOINTS);

        let service = WalletConfigService::new(config_path).expect("endpoints should be allowed");

        let configs = service.get_wallet_configs().unwrap();
        assert_eq!(configs["endpoints"]["rpc"][0], "wss://rpc-1.example.net");
        assert_eq!(
            configs["endpoints"]["explorer"],
            "https://explorer.example.net"
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn new_rejects_malformed_endpoints_at_startup() {
        let malformed = [
            r#"{ "endpoints": [] }"#,
            r#"{ "endpoints": { "rpc": [] } }"#,
            r#"{ "endpoints": { "rpc": "https://rpc.example.net" } }"#,
            r#"{ "endpoints": { "rpc": ["ftp://rpc.example.net"] } }"#,
            r#"{ "endpoints": { "rpc": ["not a url"] } }"#,
            r#"{ "endpoints": { "graphQl": ["wss://indexer.example.net"] } }"#,
            r#"{ "endpoints": { "explorer": "explorer.example.net" } }"#,
            r#"{ "endpoints": { "senoti": 42 } }"#,
        ];
        for json in malformed {
            let dir = unique_temp_dir();
            let config_path = write_flags(&dir, json);

            let err = match WalletConfigService::new(config_path) {
                Err(err) => err,
                Ok(_) => panic!("{json} must fail startup"),
            };

            assert!(matches!(err, WalletConfigsError::Invalid(_)), "{json}");
            std::fs::remove_dir_all(dir).ok();
        }
    }

    #[tokio::test]
    async fn poller_keeps_last_known_good_when_endpoints_become_invalid() {
        let dir = unique_temp_dir();
        let config_path = write_flags(&dir, WITH_ENDPOINTS);

        let service =
            WalletConfigService::new(config_path.clone()).expect("service should initialize");
        let before = service.get_wallet_configs().unwrap();

        std::fs::write(&config_path, r#"{ "endpoints": { "rpc": [] } }"#).expect("write invalid");
        tokio::time::sleep(Duration::from_millis(400)).await;

        assert_eq!(before, service.get_wallet_configs().unwrap());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn poller_reloads_flags_when_file_changes() {
        let dir = unique_temp_dir();
        let config_path = write_flags(&dir, VALID_FLAGS);

        let service =
            WalletConfigService::new(config_path.clone()).expect("service should initialize");

        std::fs::write(&config_path, UPDATED_FLAGS).expect("update flags");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let configs = service.get_wallet_configs().unwrap();
            if configs["enableTestButtons"].as_bool().unwrap()
                && configs["enableKeystoneHardwareWallet"].as_bool().unwrap()
                && !configs["enableHighSecurity"].as_bool().unwrap()
                && !configs["enableRemoteNotifications"].as_bool().unwrap()
                && !configs["enableSwap"].as_bool().unwrap()
            {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("condition not met within 2s");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn poller_keeps_last_known_good_when_json_becomes_invalid() {
        let dir = unique_temp_dir();
        let config_path = write_flags(&dir, VALID_FLAGS);

        let service =
            WalletConfigService::new(config_path.clone()).expect("service should initialize");
        let before = service.get_wallet_configs().unwrap();

        std::fs::write(&config_path, "{ invalid json }").expect("write invalid");
        tokio::time::sleep(Duration::from_millis(400)).await;

        let after = service.get_wallet_configs().unwrap();
        assert_eq!(before, after);

        std::fs::remove_dir_all(dir).ok();
    }
}
