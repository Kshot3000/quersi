use std::path::PathBuf;

use serde_json::{Map, Value};

use super::watched_json::{WatchedJson, WatchedJsonError};

pub type WalletConfigsError = WatchedJsonError;

/// The wallet feature flags: an opaque JSON object, hot-reloaded from disk.
#[derive(Debug)]
pub struct WalletConfigService {
    file: WatchedJson<Map<String, Value>>,
}

impl WalletConfigService {
    pub fn new(file_path: impl Into<PathBuf>) -> Result<Self, WalletConfigsError> {
        Ok(Self {
            file: WatchedJson::new(file_path)?,
        })
    }

    pub fn get_wallet_configs(&self) -> Result<Map<String, Value>, WalletConfigsError> {
        self.file.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
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
